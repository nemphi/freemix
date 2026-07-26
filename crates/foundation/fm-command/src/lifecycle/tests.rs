use super::*;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Domain {
    Video,
    Audio,
}

#[test]
fn lifecycle_tracks_each_domain_until_aggregate_realization() {
    let mut lifecycle = Lifecycle::<Domain, String>::new(Revision::new(12));
    lifecycle.begin_preparing().unwrap();
    lifecycle
        .schedule([
            ScheduledDomain::new(Domain::Video, 1_000),
            ScheduledDomain::new(Domain::Audio, 48_000),
        ])
        .unwrap();

    lifecycle
        .realize(Domain::Video, RuntimeGeneration::new(4))
        .unwrap();
    assert_eq!(lifecycle.phase(), LifecyclePhase::Scheduled);
    lifecycle
        .realize(Domain::Audio, RuntimeGeneration::new(9))
        .unwrap();

    assert_eq!(lifecycle.phase(), LifecyclePhase::Realized);
    assert_eq!(
        lifecycle.generation(&Domain::Video),
        Some(RuntimeGeneration::new(4))
    );
    assert_eq!(
        lifecycle
            .records()
            .iter()
            .map(|record| record.sequence.get())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert!(
        lifecycle
            .records()
            .iter()
            .all(|record| record.revision == Revision::new(12))
    );
}

#[test]
fn failed_and_superseded_are_terminal() {
    let mut failed = Lifecycle::<Domain, _>::new(Revision::new(2));
    failed.begin_preparing().unwrap();
    failed
        .fail("device lost", FailureDisposition::RetainedForRetry)
        .unwrap();
    assert_eq!(failed.phase(), LifecyclePhase::Failed);
    assert!(failed.phase().is_terminal());
    assert!(matches!(
        failed.begin_preparing(),
        Err(LifecycleError::InvalidTransition { .. })
    ));

    let mut superseded = Lifecycle::<Domain, String>::new(Revision::new(3));
    superseded.supersede(Revision::new(4)).unwrap();
    assert_eq!(superseded.phase(), LifecyclePhase::Superseded);
    assert_eq!(
        superseded.records().last().unwrap().event,
        LifecycleEvent::Superseded {
            by_revision: Revision::new(4)
        }
    );
}

#[test]
fn invalid_domain_transitions_do_not_append_events() {
    let mut lifecycle = Lifecycle::<Domain, String>::new(Revision::new(1));
    lifecycle.begin_preparing().unwrap();
    lifecycle
        .schedule([ScheduledDomain::new(Domain::Video, 20)])
        .unwrap();
    let count = lifecycle.records().len();

    assert_eq!(
        lifecycle.realize(Domain::Audio, RuntimeGeneration::new(2)),
        Err(LifecycleError::UnknownDomain)
    );
    assert_eq!(lifecycle.records().len(), count);
    lifecycle
        .realize(Domain::Video, RuntimeGeneration::new(2))
        .unwrap();
    assert_eq!(
        lifecycle.realize(Domain::Video, RuntimeGeneration::new(3)),
        Err(LifecycleError::InvalidTransition {
            from: LifecyclePhase::Realized,
            to: LifecyclePhase::Realized
        })
    );
}

#[test]
fn schedule_rejects_empty_and_duplicate_domains_without_transitioning() {
    let mut lifecycle = Lifecycle::<Domain, String>::new(Revision::new(1));
    lifecycle.begin_preparing().unwrap();

    assert_eq!(
        lifecycle.schedule(std::iter::empty()),
        Err(LifecycleError::EmptySchedule)
    );
    assert_eq!(
        lifecycle.schedule([
            ScheduledDomain::new(Domain::Video, 1),
            ScheduledDomain::new(Domain::Video, 2),
        ]),
        Err(LifecycleError::DuplicateDomain)
    );
    assert_eq!(lifecycle.phase(), LifecyclePhase::Preparing);
}
