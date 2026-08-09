use fm_auth::{Policy, Principal};
use fm_control::{ControlService, PrepareSubmitOutcome, PreparedSubmission};
use fm_persistence::{ProjectStore, StoredProject};
use fm_protocol::{
    CommandMessage, CommandResult, RejectionCode, RuntimeEventMessage, ServerIdentity,
};

use super::{
    resources::{NativeStingerMutation, PreflightOutcome},
    super::{
        AppResult, DurableExecution, NativeDaemon, ProcessShutdown, stored_project_from_snapshot,
    },
};

pub(super) struct NativeMutationFailure {
    pub(super) result: CommandResult,
    pub(super) runtime_events: Vec<RuntimeEventMessage>,
}

enum FirstPass {
    Complete(DurableExecution),
    Candidate(StoredProject),
}

pub(super) fn execute(
    control: &mut ControlService<Policy>,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    now_millis: u64,
    native: &mut NativeDaemon,
    process_shutdown: Option<&ProcessShutdown>,
) -> AppResult<Result<DurableExecution, NativeMutationFailure>> {
    let candidate = match first_pass(
        control, store, durable, principal, command, now_millis,
    )? {
        FirstPass::Complete(execution) => return Ok(Ok(execution)),
        FirstPass::Candidate(candidate) => candidate,
    };
    let preflight = native.preflight_stinger_mutation_with_ticks(
        candidate.clone(),
        control,
        server,
        process_shutdown,
    )?;
    let PreflightOutcome {
        mutation,
        runtime_events,
    } = preflight.unwrap_or(PreflightOutcome {
        mutation: Err(()),
        runtime_events: Vec::new(),
    });
    let Ok(mutation) = mutation else {
        return Ok(Err(preflight_failure(
            command,
            control,
            runtime_events,
        )));
    };
    second_pass(
        control,
        store,
        durable,
        principal,
        server,
        command,
        now_millis,
        native,
        process_shutdown,
        candidate,
        mutation,
        runtime_events,
    )
}

fn first_pass(
    control: &mut ControlService<Policy>,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    command: &CommandMessage,
    now_millis: u64,
) -> AppResult<FirstPass> {
    let preparation = control.prepare_submit(principal, command.clone(), now_millis)?;
    let prepared = match preparation {
        PrepareSubmitOutcome::Replayed(submission) => {
            return Ok(FirstPass::Complete(DurableExecution {
                submission,
                runtime_events: Vec::new(),
            }));
        }
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
    };
    if !prepared.submission().is_accepted() {
        return persist_rejected(prepared, store, durable, command)
            .map(FirstPass::Complete);
    }
    let projected = prepared.project(1)?;
    let candidate =
        stored_project_from_snapshot(durable, &projected, command, &prepared.output().result)?;
    drop(prepared);
    Ok(FirstPass::Candidate(candidate))
}

#[allow(clippy::too_many_arguments)]
fn second_pass(
    control: &mut ControlService<Policy>,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    now_millis: u64,
    native: &mut NativeDaemon,
    process_shutdown: Option<&ProcessShutdown>,
    candidate: StoredProject,
    mutation: NativeStingerMutation,
    mut runtime_events: Vec<RuntimeEventMessage>,
) -> AppResult<Result<DurableExecution, NativeMutationFailure>> {
    let preparation = control.prepare_submit(principal, command.clone(), now_millis)?;
    let prepared = match preparation {
        PrepareSubmitOutcome::Replayed(submission) => {
            native.stinger_retirements.discard(mutation)?;
            return Ok(Ok(DurableExecution {
                submission,
                runtime_events,
            }));
        }
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
    };
    if !prepared.submission().is_accepted() {
        native.stinger_retirements.discard(mutation)?;
        return persist_rejected(prepared, store, durable, command).map(Ok);
    }
    let projected = prepared.project(1)?;
    let updated =
        stored_project_from_snapshot(durable, &projected, command, &prepared.output().result)?;
    if candidate.project().stingers() != updated.project().stingers()
        || native
            .playback
            .validate_retained_byte_limit(mutation.ordinary_video_limit)
            .is_err()
    {
        drop(prepared);
        native.stinger_retirements.discard(mutation)?;
        return Ok(Err(preflight_failure(command, control, runtime_events)));
    }
    store.save(&updated)?;
    let submission = prepared.commit()?;
    native.stage_stinger_mutation(mutation);
    let realized = match native.wait_and_tick(control, server, process_shutdown)? {
        Some(events) => events,
        None => control.tick_for_shutdown(server)?.runtime_events,
    };
    runtime_events.extend(realized);
    *durable = updated;
    Ok(Ok(DurableExecution {
        submission,
        runtime_events,
    }))
}

fn persist_rejected(
    prepared: PreparedSubmission<'_, Policy>,
    store: &ProjectStore,
    durable: &mut StoredProject,
    command: &CommandMessage,
) -> AppResult<DurableExecution> {
    let projected = prepared.project(0)?;
    let updated =
        stored_project_from_snapshot(durable, &projected, command, &prepared.output().result)?;
    store.save(&updated)?;
    let submission = prepared.commit()?;
    *durable = updated;
    Ok(DurableExecution {
        submission,
        runtime_events: Vec::new(),
    })
}

fn preflight_failure(
    command: &CommandMessage,
    control: &ControlService<Policy>,
    runtime_events: Vec<RuntimeEventMessage>,
) -> NativeMutationFailure {
    NativeMutationFailure {
        result: CommandResult::Rejected {
            id: command.id.clone(),
            code: RejectionCode::Unavailable.as_str().to_owned(),
            message: "native Stinger resources could not be prepared".to_owned(),
            fields: Vec::new(),
            current_revision: control.diagnostics().current_revision,
            retryable: false,
        },
        runtime_events,
    }
}

#[cfg(test)]
pub(super) fn path_free_failure_for_test(
    command: &CommandMessage,
    control: &ControlService<Policy>,
) -> CommandResult {
    preflight_failure(command, control, Vec::new()).result
}
