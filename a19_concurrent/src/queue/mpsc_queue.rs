use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::mem::replace;
use a19_core::pow2::PowOf2;
use std::thread;
use crate::queue::{ConcurrentQueue, PaddedUsize};
use std::cell::UnsafeCell;
use std::sync::Arc;

pub struct MpscNode<T> {
    id: AtomicUsize,
    value: Option<T>,
}
 
pub struct MpscQueueWrap<T> {
    queue: Arc<UnsafeCell<MpscQueue<T>>>
}

/// Forces result move guards.
pub struct MpscQueueReceive<T> {
    queue: Arc<UnsafeCell<MpscQueue<T>>>
}

unsafe impl<T> Sync for MpscQueueWrap<T> {}
unsafe impl<T> Send for MpscQueueWrap<T> {}

impl<T> MpscQueueWrap<T> {

    fn new(queue_size: usize) -> (MpscQueueWrap<T>, MpscQueueReceive<T>) {
        let queue = Arc::new(UnsafeCell::new(MpscQueue::new(queue_size)));
        let send_queue = queue.clone();
        (MpscQueueWrap {
            queue
        },
        MpscQueueReceive {
            queue: send_queue
        })
    }

    fn offer(&self, v: T) -> bool {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.offer(v)
        }
    }
}

unsafe impl<T> Send for MpscQueueReceive<T> {}

impl <T> MpscQueueReceive<T> {

    fn poll(&self) -> Option<T> {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.poll()
        }
    }

    fn drain(&self, act: fn(T), limit: usize) -> usize {
        unsafe {
            let queue = &mut *self.queue.get();
            queue.drain(act, limit)
        }
    }
}

/// A thread pool that is safe for multiple threads to read from but only a single thread to read.
struct MpscQueue<T> {
    mask: usize,
    ring_buffer: Vec<MpscNode<T>>,
    capacity: usize,
    sequence_number: PaddedUsize,
    producer: PaddedUsize
}

unsafe impl<T> Send for MpscQueue<T> {}
unsafe impl<T> Sync for MpscQueue<T> {}

impl<T> MpscQueue<T> {

    fn new(queue_size: usize) -> Self {
        let power_of_2 = queue_size.round_to_power_of_two();
        let mut queue = MpscQueue {
            ring_buffer: Vec::with_capacity(power_of_2),
            capacity: power_of_2,
            mask: power_of_2 - 1,
            sequence_number: PaddedUsize {
                padding: [0; 15],
                counter: AtomicUsize::new(1)
            },
            producer: PaddedUsize {
                padding: [0; 15],
                counter: AtomicUsize::new(1)
            }
        };
        for _ in 0..power_of_2 {
            let node = MpscNode {
                id: AtomicUsize::new(0),
                value: None
            };
            queue.ring_buffer.push(node);
        }
        queue
    }

    #[inline]
    fn pos(&self, index: usize) -> usize {
        index & self.mask
    }
}

impl<T> ConcurrentQueue<T> for MpscQueue<T> {

    /// Used to poll the queue and moves the value to the option if there is a value.
    fn poll(&mut self) -> Option<T> {
        let mut i: u64 = 0;
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
                        self.sequence_number.counter.store(s_index + 1, Ordering::Relaxed);
                        let v = replace(&mut node.value, Option::None);
                        node.id.store(0, Ordering::Relaxed);
                        break v
                    } else {
                        i = i + 1;
                        if i > 1_000_000_000 {
                            panic!(format!("Got stuck on {}:{}:{}:{}:{}", s_index, p_index, node_id, last_pos, self.capacity))
                        }
                    }
                }
            } else {
                break None
            }
        }
    }

    /// A quick way to drain all of the values from the queue.
    /// # Arguments
    /// `act` - The action to run against the queue.
    /// # Returns
    /// The number of items that where returned.
    fn drain(&mut self, act: fn(T), limit: usize) -> usize {
        loop {
            let p_index = self.producer.counter.load(Ordering::Relaxed);
            let s_index = self.sequence_number.counter.load(Ordering::Relaxed);
            if p_index <= s_index {
                break 0
            } else {
                let elements_left = p_index - s_index;
                let request = limit.min(elements_left);
                // Have to do this a little bit different.
                self.sequence_number.counter.store(s_index + request, Ordering::Relaxed);
                for i in 0..request {
                    let mut fail_count: u64 = 0;
                    loop {
                        let pos = self.pos(s_index + i);
                        let node = unsafe {self.ring_buffer.get_unchecked_mut(pos)}; 
                        let node_id = node.id.load(Ordering::Acquire);
                        if node_id == s_index + i {
                            let v = replace(&mut node.value, Option::None);
                            node.id.store(0, Ordering::Relaxed);
                            match v {
                                None => {
                                    panic!("Found a None!")
                                },
                                Some(t_value) => {
                                    act(t_value)
                                }
                            }
                            break
                        } else {
                            thread::yield_now();
                            fail_count = fail_count + 1;
                            if fail_count > 1_000_000_000 {
                                panic!("Failed to get the node!");
                            }
                        }
                    }
                }
                break request
            }
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
            if p_index < capacity
                || p_index - capacity < c_index {
                    let pos = self.pos(p_index);
                    let mut node = unsafe {self.ring_buffer.get_unchecked_mut(pos)};
                    if node.id.load(Ordering::Acquire) == 0 {
                        match self.producer.counter.compare_exchange_weak(p_index, p_index + 1, Ordering::Relaxed, Ordering::Relaxed) {
                            Ok(_) => {
                                node.value = Some(value);
                                node.id.store(p_index, Ordering::Relaxed);
                                break true
                            },
                            Err(_) => {
                            }
                        }
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

    use std::thread;
    use crate::queue::mpsc_queue::{
        MpscQueue,
        MpscQueueWrap
    };
    use crate::queue::ConcurrentQueue;
    use std::sync::Arc;
    use std::vec::Vec;

    #[test]
    pub fn create_queue_test() {
        let mut queue: MpscQueue<u64> = MpscQueue::new(128);
        assert_eq!(128, queue.ring_buffer.len());

        queue.offer(1);
        let result = queue.poll();
        assert_eq!(Some(1), result);
    }

    #[test]
    pub fn use_thread_queue_test() {
        time_test!();
        let (write, send) = MpscQueueWrap::<usize>::new(1_000_000); 
        let queue: Arc<MpscQueueWrap<usize>> = Arc::new(write);
        let write_thread_num = 2;
        let mut write_threads: Vec<thread::JoinHandle<_>> = Vec::with_capacity(write_thread_num); 
        let spins: usize = 10_000_000;
        for _ in 0..write_thread_num {
            let write_queue = queue.clone();
            let write_thread = thread::spawn(move || {
                for i in 0..(spins / write_thread_num) {
                    while !write_queue.offer(i) {
                        thread::yield_now()
                    }
                }
            });
            write_threads.push(write_thread);
        }

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
                            break
                        }
                    },
                    _ => {
                        thread::yield_now();
                    }
                }
            } 
        });
        read_threads.push(read_thread);

        for num in 0..write_thread_num {
            write_threads.remove(write_thread_num - num - 1).join().unwrap();
        }
        for num in 0..thread_num {
            read_threads.remove(thread_num - num - 1).join().unwrap();
        }
    }

    #[test]
    pub fn use_thread_queue_test_drain() {
        time_test!();
        let (write, send) = MpscQueueWrap::<usize>::new(1_000_000); 
        let queue: Arc<MpscQueueWrap<usize>> = Arc::new(write);
        let write_queue = queue.clone();
        let spins: usize = 10_000_000;
        let write_thread = thread::spawn(move || {
            for i in 0..spins {
                while !write_queue.offer(i) {
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
                let result = send.drain(|_| {
                }, 1000);
                count = count + result;
                if count == 0 {
                    thread::yield_now();
                }
                if count > (read_spins - 1000 * thread_num) {
                    break
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