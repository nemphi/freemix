# Functional MVP acceptance contract

The first runnable FreeMix milestone is a deterministic headless switcher. It is
complete when one executable can:

1. create a show with two simulated color inputs;
2. print stable project, revision, frame, Program, and Preview state;
3. select Preview through an idempotent revisioned command;
4. realize Cut on the next simulated frame boundary;
5. realize Fade over an exact requested frame count;
6. reject duplicate, stale-revision, and invalid-input commands without partial
   state changes;
7. save the show atomically and reload it in a new process without losing the
   accepted revision or realized switcher state; and
8. execute a scripted end-to-end scenario in CI without wall-clock sleeps,
   devices, network access, or GPU access.

This is the Phase 1 simulated vertical slice from the implementation roadmap.
It proves the state/command/media boundary used by later GPU, audio, capture,
recording, networking, and UI phases; it does not claim those later adapters are
implemented.
