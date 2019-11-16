use std::sync::Arc;
use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::cmp::Eq;
use std::sync::atomic::{AtomicU32, Ordering, AtomicPtr};
use std::thread;

const READER: u32 = 1;
const WRITER: u32 = 2;

/// Applies a change stream of events on the a map.
pub trait ApplyChanges<K: Hash + Eq, V, E> {
    /// Applies a change.
    fn apply(&self, map: &mut HashMap<K, V>, event: &E);
}

/// The container for the map.
/// K - Is the key in the hash map.
struct MapContainer<K: Hash + Eq, V, E> {
    /// The hash map.
    map: HashMap<K, V>,
    /// The the number of reads currently on the map.
    reader_count: AtomicU32,
    /// If this map is currently a reader or writer.
    state: AtomicU32,
    /// The event stream to apply when we make it a writer.
    event_stream: VecDeque<E>,
}

impl<K: Hash + Eq, V, E> MapContainer<K, V, E> {
    /// Used to create a new map container.
    /// # Arguments
    /// `starting_map` - The starting map to use for the container.
    /// `initial_state` - The initial state of the map.
    /// `event_stream_size` - The event stream size.
    fn new(
        starting_map: HashMap<K, V>,
        initial_state: u32,
        event_stream_size: usize) -> Self {
        MapContainer {
            map: starting_map,
            reader_count: AtomicU32::new(0),
            state: AtomicU32::new(initial_state),
            event_stream: VecDeque::with_capacity(event_stream_size),
        }
    }
}

/// A wrapper for the reader so we can have proper threading access.
pub struct MrswMapReader<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> {
    map: Arc<UnsafeCell<MrswMap<K, V, E, TApplyChange>>>,
}

unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Sync for MrswMapReader<K, V, E, TApplyChange> {}
unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Send for MrswMapReader<K, V, E, TApplyChange> {}

impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> MrswMapReader<K, V, E, TApplyChange> {

    pub fn get<R>(&self, key: K, act: fn(Option<&V>) -> R) -> R {
        unsafe {
            let map = &mut *self.map.get();
            map.get(key, act)
        }
    }
}

unsafe impl<K: Hash + Eq, V, E> Sync for MapContainer<K, V, E> { }
unsafe impl<K: Hash + Eq, V, E> Send for MapContainer<K, V, E> { }

/// The multi reader, single writer map.
pub struct MrswMap<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> {
    current_reader: AtomicPtr<MapContainer<K, V, E>>,
    /// The first map.
    map1: MapContainer<K, V, E>,
    /// The second map.
    map2: MapContainer<K, V, E>,
    /// The function that is used to apply the changes to the hashmap.
    apply_change: TApplyChange,
}

impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> MrswMap<K, V, E, TApplyChange> {
    /// Creates a new MrswMap with the specified capacity.
    /// # Arguments
    /// `map1` - The starting map.  This needs to have the same data as map2 but be a copy.  Since we don't have an easy
    /// way to make a deep clone, I think it is easier just to have the coder provide the maps.
    /// `map2` - The second starting map.
    /// `apply_change` - Used to apply the changes onto the map.
    pub fn new(
        map1: HashMap<K, V>,
        map2: HashMap<K, V>,
        apply_change: TApplyChange) -> (MrswMapReader<K, V, E, TApplyChange>, MrswMapWriter<K, V, E, TApplyChange>) {
        let mut reader = MapContainer::new(
            map1,
            READER,
            1_024);
        let ptr = AtomicPtr::<MapContainer<K, V, E>>::new(&mut reader);
        let mrsp_map = Arc::new(UnsafeCell::new(MrswMap {
            current_reader: ptr,
            map1: reader,
            map2: MapContainer::new(
                map2,
                WRITER,
                1_024
            ),
            apply_change
        }));
        (
            MrswMapReader {
                map: mrsp_map.clone(),
            },
            MrswMapWriter {
                map: mrsp_map
            }
        )
    }

    /// Used to apply a change to a map.
    /// # Arguments
    /// `map` - The map to apply the change to.
    /// `event` - The event to apply to the map.
    fn apply_int(
        apply_change: &TApplyChange,
        map: &mut MapContainer<K, V, E>,
        event: &E) {
        apply_change.apply(&mut map.map, event);
    }

    /// Used to add an event to the reader that will be processed when it is commited.
    /// # Arguments
    /// `map` - The reader map.  This isn't validated.
    /// `event` - The event to add to the queue.
    fn add_event_int(
        map: &mut MapContainer<K, V, E>,
        event: E) {
        map.event_stream.push_back(event);
    }

}

pub struct MrswMapWriter<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> {
    map: Arc<UnsafeCell<MrswMap<K, V, E, TApplyChange>>>
}

unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Sync for MrswMapWriter<K, V, E, TApplyChange> {}
unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Send for MrswMapWriter<K, V, E, TApplyChange> {}

impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> MrswMapWriter<K, V, E, TApplyChange> {
    
    pub fn add_event(&self, event: E) {
        unsafe {
            let map = &mut *self.map.get();
            map.add_event(event);
        }    
    }

    pub fn commit(&self) {
        unsafe {
            let map = &mut *self.map.get();
            map.commit();
        }
    }
}


/// The calls to this are not thread safe and must be done using a single thread.  If you want
/// multiple writers you need to use a mutex to achieve this.
pub trait WriterMap<K: Hash + Eq, V, E> {
    /// Adds an event to the map to be processed.
    fn add_event(&mut self, event: E);

    /// Used to commit the events after we are done.  All of the events are done processing when
    /// this is called.
    fn commit(&mut self);
}

impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> WriterMap<K, V, E> for MrswMap<K, V, E, TApplyChange> {
    /// Adds an event to the map to be processed.
    fn add_event(&mut self, event: E) {
        let (writer, reader) = if self.map1.state.load(Ordering::Relaxed) == WRITER {
            (&mut self.map1, &mut self.map2)
        } else {
            (&mut self.map2, &mut self.map1)
        };
        MrswMap::apply_int(&self.apply_change, writer, &event);
        MrswMap::<K, V, E, TApplyChange>::add_event_int(reader, event);
    }

    fn commit(&mut self) {
        let (writer, reader) = if self.map1.state.load(Ordering::Relaxed) == WRITER {
            (&mut self.map1, &mut self.map2)
        } else {
            (&mut self.map2, &mut self.map1)
        };
        // Full memory barrier hear so we don't accidently have a thread read the wrong writer
        // value.  Need to do this immediately so we 
        writer.state.store(READER, Ordering::Relaxed);
        reader.state.store(WRITER, Ordering::Relaxed);
        // Need to do an atomic store of the current writer.
        self.current_reader.store(reader, Ordering::SeqCst);
        loop {
            // Wait for the reader count to go to zero.
            if reader.reader_count.load(Ordering::Relaxed) == 0 {
                break
            } else {
                thread::yield_now();
            }
        }

        // Apply the events to the reader map.
        loop {
            let event = reader.event_stream.pop_front();
            match event {
                Some(e) => {
                    MrswMap::apply_int(&self.apply_change, reader, &e)
                }, 
                None => {
                    break
                }
            }
        }
    }
}

/// Used to get a value out of the map.
pub trait ReaderMap<K: Hash + Eq, V> {

    /// Gets a value out of the map.  Have to have it function based since we need to know when
    /// they are done reading the data.
    /// # Arguments
    /// `key` - The key to get the value.
    /// `act` - The action to run with the value that returns the specified result.
    fn get<R>(&mut self, key: K, act: fn(Option<&V>) -> R) -> R;
}
impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> ReaderMap<K, V> for MrswMap<K, V, E, TApplyChange> {

    fn get<R>(&mut self, key: K, act: fn(Option<&V>) -> R) -> R {
        let reader = self.current_reader.load(Ordering::SeqCst);
        unsafe {(*reader).reader_count.fetch_add(1, Ordering::Relaxed)};
        let elem = unsafe{(*reader).map.get(&key)};
        let r = act(elem);
        unsafe {(*reader).reader_count.fetch_add(1, Ordering::Relaxed)};
        r
    }
}
unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Sync for MrswMap<K, V, E, TApplyChange> { }
unsafe impl<K: Hash + Eq, V, E, TApplyChange: ApplyChanges<K, V, E>> Send for MrswMap<K, V, E, TApplyChange> { }

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use crate::map::mrsw_map::{MrswMap, WriterMap, ApplyChanges, ReaderMap};

    enum TestEvent {
        Add{key: u64, value: String}
    }

    struct TestApplyChange {

    }

    impl ApplyChanges<u64, String, TestEvent> for TestApplyChange {

        fn apply(&self, map: &mut HashMap<u64, String>, event: &TestEvent) {
            match event {
                TestEvent::Add{key: k, value: s} => {
                    map.insert(*k, s.clone());
                }
            }
        }
    }


    #[test]
    pub fn create_mrsw_map() {
        let apply_change = TestApplyChange {

        };
        let (reader, writer) = MrswMap::new(
            HashMap::with_capacity(10),
            HashMap::with_capacity(10),
            apply_change
        );
        writer.add_event(TestEvent::Add{key: 1, value: "Hi".to_owned()});
        writer.commit();
        let r = reader.get(1, |e| {
            match e {
                Some(r) => {
                    r.clone()
                },
                None => {
                    "".to_owned()
                }
            }
        });
        assert_eq!("Hi", &r);
    }
}