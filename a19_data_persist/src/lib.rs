pub mod file;
pub mod raft;
pub mod message_stream;

use futures::channel::oneshot;

/// Represents the committing of data as a future.  Since this will be distributed at some point we
/// want to be able to batch events.
type CommitFuture<TOUT> = oneshot::Sender<TOUT>;

/// Used to presist an event stream to a persited store like disk.  Need to also decided how we are
/// going to stream the events.
pub trait PersitEventStream {
    /// Commits the data to a stream.  It doesn't care about the type.  Returns the transaction
    /// number.  The add needs to allow multiple writers to prevent a bottleneck.
    /// # Arguments.
    /// `value` - The value to save to persist.  This doesn't have to happen immediately.  Need to
    /// define your own header and for the message when you save it.  The point of this stream is
    /// it doesn't care.
    fn add_change(&self, value: &[u8]) -> CommitFuture<u64>;

    /// Used to replay the events from a specified number.
    /// #Arguments
    /// commit_key - The commit key to read the events from.
    fn get_events_from(&self, commit_key: u64) -> &[u8];
}

#[cfg(test)]
mod tests {}
