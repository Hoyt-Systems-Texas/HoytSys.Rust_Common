use crate::queue::{ConcurrentQueue, PaddedUsize};
use a19_core::pow2::PowOf2;
use std::cell::UnsafeCell;
use std::mem::replace;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::vec::Vec;

pub struct SpscNode<T> {
    id: AtomicUsize,
    value: Option<T>,
}

pub struct SpscQueueSendWrap<T> {
    queue: Arc<UnsafeCell<SpscQueue<T>>>,
}

unsafe impl<T> Send for SpscQueueSendWrap<T> {}

impl<T> SpscQueueSendWrap<T> {
    pub fn new(queue_size: usize) -> (SpscQueueSendWrap<T>, SpscQueueReceiveWrap<T>) {
        let queue = Arc::new(UnsafeCell::new(SpscQueue::new(queue_size)));
        let send_queue = queue.clone();
        (
            SpscQueueSendWrap { queue: send_queue },
            SpscQueueReceiveWrap { queue },
        )
    }

    pub fn offer(&self, v: T) -> bool {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.offer(v)
        }
    }
}

pub struct SpscQueueReceiveWrap<T> {
    queue: Arc<UnsafeCell<SpscQueue<T>>>,
}

unsafe impl<T> Send for SpscQueueReceiveWrap<T> {}

impl<T> SpscQueueReceiveWrap<T> {
    pub fn poll(&self) -> Option<T> {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.poll()
        }
    }

    pub fn drain(&self, act: fn(T), limit: usize) -> usize {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.drain(act, limit)
        }
    }

    pub fn peek(&'_ self) -> Option<&'_ T> {
        let queue = unsafe { &mut *self.queue.get() };
        queue.peek()
    }
}

struct SpscQueue<T> {
    mask: usize,
    ring_buffer: Vec<SpscNode<T>>,
    capacity: usize,
    sequence_number: PaddedUsize,
    producer: PaddedUsize,
}

unsafe impl<T> Send for SpscQueue<T> {}
unsafe impl<T> Sync for SpscQueue<T> {}

impl<T> SpscQueue<T> {
    fn new(queue_size: usize) -> Self {
        let power_of_2 = queue_size.round_to_power_of_two();
        let mut queue = SpscQueue {
            ring_buffer: Vec::with_capacity(power_of_2),
            capacity: power_of_2,
            mask: power_of_2 - 1,
            sequence_number: PaddedUsize {
                padding: [0; 15],
                counter: AtomicUsize::new(1),
            },
            producer: PaddedUsize {
                padding: [0; 15],
                counter: AtomicUsize::new(1),
            },
        };
        for _ in 0..power_of_2 {
            let node = SpscNode {
                id: AtomicUsize::new(0),
                value: None,
            };
            queue.ring_buffer.push(node);
        }

        queue
    }

    #[inline]
    fn pos(&self, index: usize) -> usize {
        index & self.mask
    }

    fn peek(&'_ self) -> Option<&'_ T> {
        let s_index = self.sequence_number.counter.load(Ordering::Relaxed);
        let p_index = self.producer.counter.load(Ordering::Relaxed);
        if p_index > s_index {
            let last_pos = self.pos(s_index);
            let node = unsafe { self.ring_buffer.get_unchecked(last_pos) };
            let node_id = node.id.load(Ordering::Acquire);
            // Verify the node id matches the index id.
            if node_id == s_index {
                match &node.value {
                    Some(value) => Some(value),
                    None => None,
                }
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl<T> ConcurrentQueue<T> for SpscQueue<T> {
    /// Used to poll the queue and moves the value to the option if there is a value.
    fn poll(&mut self) -> Option<T> {
        loop {
            let s_index = self.sequence_number.counter.load(Ordering::Relaxed);
            let p_index = self.producer.counter.load(Ordering::Relaxed);
            if p_index > s_index {
                unsafe {
                    let last_pos = self.pos(s_index);
                    let node = self.ring_buffer.get_unchecked_mut(last_pos);
                    let node_id = node.id.load(Ordering::Acquire);
                    // Verify the node id matches the index id.
                    if node_id == s_index {
                        // Try and claim the slot.
                        self.sequence_number
                            .counter
                            .store(s_index + 1, Ordering::Relaxed);
                        let v = replace(&mut node.value, Option::None);
                        node.id.store(0, Ordering::Relaxed);
                        break v;
                    } else {
                        // Go around again.
                    }
                }
            } else {
                break None;
            }
        }
    }

    /// A quick way to drain all of the values from the queue.
    /// # Arguments
    /// `act` - The action to run against the queue.
    /// # Returns
    /// The number of items that where returned.
    fn drain(&mut self, act: fn(T), limit: usize) -> usize {
        let p_index = self.producer.counter.load(Ordering::Relaxed);
        let s_index = self.sequence_number.counter.load(Ordering::Relaxed);
        if p_index <= s_index {
            0
        } else {
            let elements_left = p_index - s_index;
            let request = limit.min(elements_left);
            // Have to do this a little bit different.
            self.sequence_number
                .counter
                .store(s_index + request, Ordering::Relaxed);
            for i in 0..request {
                loop {
                    let pos = self.pos(s_index + i);
                    let node = unsafe { self.ring_buffer.get_unchecked_mut(pos) };
                    let node_id = node.id.load(Ordering::Acquire);
                    if node_id == s_index + i {
                        let v = replace(&mut node.value, Option::None);
                        node.id.store(0, Ordering::Relaxed);
                        match v {
                            None => panic!("Found a None!"),
                            Some(t_value) => act(t_value),
                        }
                        break;
                    } else {
                        thread::yield_now();
                    }
                }
            }
            request
        }
    }

    /// Offers a value to the queue.  Returns true if the value was successfully added.
    /// # Arguments
    /// `value` - The vale to add to the queue.
    fn offer(&mut self, value: T) -> bool {
        let capacity = self.capacity;
        loop {
            let p_index = self.producer.counter.load(Ordering::Relaxed);
            let c_index = self.sequence_number.counter.load(Ordering::Relaxed);
            if p_index < capacity || p_index - capacity < c_index {
                let pos = self.pos(p_index);
                let mut node = unsafe { self.ring_buffer.get_unchecked_mut(pos) };
                if node.id.load(Ordering::Acquire) == 0 {
                    self.producer.counter.store(p_index + 1, Ordering::Relaxed);
                    node.value = Some(value);
                    node.id.store(p_index, Ordering::Relaxed);
                    break true;
                } else {
                    thread::yield_now();
                }
            } else {
                break false;
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::queue::spsc_queue::{SpscQueue, SpscQueueReceiveWrap, SpscQueueSendWrap};
    use crate::queue::ConcurrentQueue;
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;
    use time_test::time_test;

    #[test]
    pub fn create_queue_test() {
        let mut queue: SpscQueue<u64> = SpscQueue::new(128);
        assert_eq!(128, queue.ring_buffer.len());

        queue.offer(1);
        let result = queue.poll();
        assert_eq!(Some(1), result);
    }

    #[test]
    pub fn use_thread_queue_test() {
        time_test!();
        let (write, send) = SpscQueueSendWrap::<usize>::new(1_000_000);
        let spins: usize = 10_000_000;
        let write_thread = thread::spawn(move || {
            for i in 0..spins {
                while !write.offer(i) {
                    thread::yield_now()
                }
            }
        });

        let thread_num: usize = 1;
        let mut read_threads: Vec<thread::JoinHandle<_>> = Vec::with_capacity(thread_num);
        let read_spins = spins / thread_num;
        let read_thread = thread::spawn(move || {
            let mut count = 0;
            loop {
                let result = send.poll();
                match result {
                    Some(_) => {
                        count = count + 1;
                        if count == read_spins {
                            break;
                        }
                    }
                    _ => {
                        thread::yield_now();
                    }
                }
            }
        });
        read_threads.push(read_thread);

        write_thread.join();
        for num in 0..thread_num {
            read_threads.remove(thread_num - num - 1).join().unwrap();
        }
    }

    #[test]
    pub fn use_thread_queue_test_drain() {
        time_test!();
        let (write, send) = SpscQueueSendWrap::<usize>::new(1_000_000);
        let spins: usize = 10_000_000;
        let write_thread = thread::spawn(move || {
            for i in 0..spins {
                while !write.offer(i) {
                    thread::yield_now()
                }
            }
        });

        let thread_num: usize = 1;
        let mut read_threads: Vec<thread::JoinHandle<_>> = Vec::with_capacity(thread_num);
        let read_spins = spins / thread_num;
        let read_thread = thread::spawn(move || {
            let mut count = 0;
            loop {
                let result = send.drain(|_| {}, 1000);
                count = count + result;
                if count == 0 {
                    thread::yield_now();
                }
                if count > (read_spins - 1000 * thread_num) {
                    break;
                }
            }
        });
        read_threads.push(read_thread);

        write_thread.join().unwrap();
        for num in 0..thread_num {
            read_threads.remove(thread_num - num - 1).join().unwrap();
        }
    }
}
