/// Runs synchronous disk work, such as reading or syncing a session log,
/// without stalling the other tasks of a multi-thread runtime's worker. A
/// current-thread runtime has no other worker to hand them to, and a thread
/// outside a runtime has nothing to stall, so the work runs in place there.
/// Nested calls run in place too, on the thread the outer call already freed.
pub(crate) fn off_worker<T>(work: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|runtime| runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}
