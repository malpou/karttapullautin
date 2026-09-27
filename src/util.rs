use std::{
    collections::VecDeque,
    fmt::Debug,
    io::{self, BufRead},
    path::Path,
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

use anyhow::Context;
use log::debug;
use rand::{SeedableRng, rngs::Xoshiro256PlusPlus};

use crate::io::fs::FileSystem;

/// Iterates over the lines in a file and calls the callback with a &str reference to each line.
/// This function does not allocate new strings for each line, as opposed to using
/// [`io::BufReader::lines()`] as in [`read_lines`].
pub fn read_lines_no_alloc<P>(
    fs: &impl FileSystem,
    filename: P,
    mut line_callback: impl FnMut(&str),
) -> io::Result<()>
where
    P: AsRef<Path> + Debug,
{
    debug!("Reading lines from {filename:?}");
    let start = Instant::now();

    let mut reader = fs.open(filename)?;

    let mut line_buffer = String::new();
    let mut line_count: u32 = 0;
    let mut byte_count: usize = 0;
    loop {
        let bytes_read = reader.read_line(&mut line_buffer)?;

        if bytes_read == 0 {
            break;
        }

        line_count += 1;
        byte_count += bytes_read;

        // the read line contains the newline delimiter, so we need to trim it off
        let line = line_buffer.trim_end();
        line_callback(line);
        line_buffer.clear();
    }

    let elapsed = start.elapsed();
    if line_count == 0 {
        debug!("No lines read");
        return Ok(());
    }
    debug!(
        "Read {} lines in {:.2?} ({:.2?}/line), total {} bytes ({:.2} bytes/second, {:?}/byte, {:.2} bytes/line)",
        line_count,
        elapsed,
        elapsed / line_count,
        byte_count,
        byte_count as f64 / elapsed.as_secs_f64(),
        elapsed / byte_count as u32,
        byte_count as f64 / line_count as f64,
    );

    Ok(())
}

/// Helper struct to time operations. Keeps track of the total time taken until the object is
/// dropped, as well as timing between individual sub-sections of the operation.
/// Timing information is printed using debug level log messages.
pub struct Timing {
    name: &'static str,
    start: Instant,
    current_section: Option<TimingSection>,
}

struct TimingSection {
    name: &'static str,
    start: Instant,
}

impl Timing {
    /// Start a new timing from now.
    pub fn start_now(name: &'static str) -> Self {
        debug!("[timing: {name}] Starting timing");
        Self {
            name,
            start: Instant::now(),
            current_section: None,
        }
    }

    /// Start a new timing section. This will end any already existing sections.
    pub fn start_section(&mut self, name: &'static str) {
        let now = self.end_section().unwrap_or(Instant::now());

        debug!("[timing: {}] Entering section '{}'", self.name, name);

        self.current_section = Some(TimingSection { name, start: now })
    }

    /// Ends the currnently active section and returns its end time, or does nothing
    /// if no section is active and returns `None`.
    pub fn end_section(&mut self) -> Option<Instant> {
        if let Some(s) = self.current_section.take() {
            //
            let now = Instant::now();
            debug!(
                "[timing: {}] Leaving section '{}', which took {:.3?}",
                self.name,
                s.name,
                now - s.start
            );
            Some(now)
        } else {
            None
        }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        self.end_section();

        debug!(
            "[timing: {}] Stopping timing. Total: {:.3?} elapsed.",
            self.name,
            self.start.elapsed()
        );
    }
}

/// Helper to read an object serialized to disk
pub fn read_object<R: std::io::Read, O: serde::de::DeserializeOwned>(
    mut reader: R,
) -> anyhow::Result<O> {
    let value: bincode_next::serde::Compat<O> =
        bincode_next::decode_from_std_read(&mut reader, bincode_next::config::standard())
            .context("deserializing from file")?;
    Ok(value.0)
}

/// Helper to write an object to disk
pub fn write_object<W: std::io::Write, O: serde::Serialize>(
    mut writer: W,
    value: &O,
) -> anyhow::Result<()> {
    bincode_next::encode_into_std_write(
        bincode_next::serde::Compat(value),
        &mut writer,
        bincode_next::config::standard(),
    )
    .context("serializing to file")?;
    Ok(())
}

/// The generator for `thinfactor` thinning of the points `source` contributes to `tile`
/// (both tile names). A tile's own points use the tile name alone, so the batch job thins
/// them exactly as the single job does; a neighbour's padding gets its own stream, so the
/// result does not depend on the order the tiles are read in.
pub fn thinning_rng(tile: &str, source: &str) -> Xoshiro256PlusPlus {
    if source == tile {
        seeded_rng(tile)
    } else {
        seeded_rng(&format!("{tile}<{source}"))
    }
}

/// The generator for `cliffthin` sampling in `tile`, apart from its point thinning.
pub fn cliff_thinning_rng(tile: &str) -> Xoshiro256PlusPlus {
    seeded_rng(&format!("{tile} cliffs"))
}

/// A random number generator seeded from `key`, so that thinning picks the same points on
/// every run of one build (rand keeps sampled values stable only within a minor
/// version). The seed is the 64-bit FNV-1a hash of `key`: std's hashers are not stable
/// across Rust versions. The generator is named, not `SmallRng`, whose algorithm differs
/// by platform and rand version.
fn seeded_rng(key: &str) -> Xoshiro256PlusPlus {
    let hash = key.bytes().fold(0xcbf29ce484222325_u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    Xoshiro256PlusPlus::seed_from_u64(hash)
}

/// A bounded Single-Producer-Multiple-Consumer queue.
///
/// [`Producer::push`] blocks when the queue contains `capacity` items, resuming once a consumer
/// pops an item. This provides backpressure so the producer cannot outpace consumers.
pub fn make_bounded_queue<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity > 0, "queue capacity must be at least 1");
    let inner = Inner {
        inner: Mutex::new(InnerMut {
            queue: VecDeque::new(),
            has_closed: false,
        }),
        capacity,
        var_has_items: Condvar::new(),
        var_has_space: Condvar::new(),
    };
    let inner = Arc::new(inner);

    let producer = Producer {
        inner: Arc::clone(&inner),
    };
    let consumer = Consumer { inner };
    (producer, consumer)
}

struct Inner<T> {
    inner: Mutex<InnerMut<T>>,
    capacity: usize,
    var_has_items: Condvar,
    var_has_space: Condvar,
}

struct InnerMut<T> {
    queue: VecDeque<T>,
    has_closed: bool,
}

pub struct Producer<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Producer<T> {
    /// Pushes an item to the queue. Blocks if the queue is at capacity until space is available.
    pub fn push(&self, item: T) {
        let start = Instant::now();
        let mut inner = self.inner.inner.lock().unwrap();
        while inner.queue.len() >= self.inner.capacity {
            inner = self.inner.var_has_space.wait(inner).unwrap();
        }
        inner.queue.push_back(item);
        self.inner.var_has_items.notify_one();
        log::debug!("Waited {:.2?} to push item to queue", start.elapsed());
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.inner.lock().unwrap();
        inner.has_closed = true;
        self.inner.var_has_items.notify_all();
    }
}

#[derive(Clone)]
pub struct Consumer<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Consumer<T> {
    /// Pops an item from the queue. Returns `None` if the queue has been closed and is empty.
    pub fn pop(&self) -> Option<T> {
        let start = Instant::now();
        let mut inner = self.inner.inner.lock().unwrap();
        loop {
            if let Some(item) = inner.queue.pop_front() {
                self.inner.var_has_space.notify_one();
                log::debug!("Waited {:.2?} to pop item from queue", start.elapsed());
                return Some(item);
            }
            if inner.has_closed {
                return None;
            }
            inner = self.inner.var_has_items.wait(inner).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn seeded_rng_is_stable_per_key() {
        use rand::Rng;
        let draw = |key| {
            let mut rng = seeded_rng(key);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw("tile_a"), draw("tile_a"));
        assert_ne!(draw("tile_a"), draw("tile_b"));
    }

    #[test]
    fn thinning_streams_are_keyed_by_tile_and_source() {
        use rand::Rng;
        let draw = |mut rng: Xoshiro256PlusPlus| (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>();
        // a tile's own points: the single job's stream
        assert_eq!(draw(thinning_rng("a", "a")), draw(seeded_rng("a")));
        // a neighbour's padding: its own stream per (tile, source) pair, whatever the order
        assert_eq!(draw(thinning_rng("a", "b")), draw(thinning_rng("a", "b")));
        assert_ne!(draw(thinning_rng("a", "b")), draw(thinning_rng("a", "a")));
        assert_ne!(draw(thinning_rng("a", "b")), draw(thinning_rng("b", "a")));
        assert_ne!(draw(cliff_thinning_rng("a")), draw(thinning_rng("a", "a")));
    }

    #[test]
    fn test_queue() {
        let (producer, consumer) = make_bounded_queue(usize::MAX);

        let producer_thread = thread::spawn(move || {
            for i in 0..10 {
                producer.push(i);
            }
        });

        let consumer_thread = thread::spawn(move || {
            let mut items = Vec::new();
            while let Some(item) = consumer.pop() {
                items.push(item);
            }
            items
        });

        producer_thread.join().unwrap();
        let items = consumer_thread.join().unwrap();
        assert_eq!(items, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_bounded_queue_backpressure() {
        // Queue of capacity 2: producer should block until consumers drain items.
        let (producer, consumer) = make_bounded_queue(2);

        let producer_thread = thread::spawn(move || {
            for i in 0..10 {
                producer.push(i);
            }
        });

        let consumer_thread = thread::spawn(move || {
            let mut items = Vec::new();
            while let Some(item) = consumer.pop() {
                items.push(item);
            }
            items
        });

        producer_thread.join().unwrap();
        let items = consumer_thread.join().unwrap();
        assert_eq!(items, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_multiple_consumer() {
        let (producer, consumer) = make_bounded_queue(usize::MAX);

        let producer_thread = thread::spawn(move || {
            for i in 0..10 {
                producer.push(i);
            }
        });

        let consumer_thread1 = thread::spawn({
            let consumer = consumer.clone();
            move || {
                let mut items = Vec::new();
                while let Some(item) = consumer.pop() {
                    items.push(item);
                }
                items
            }
        });

        let consumer_thread2 = thread::spawn({
            let consumer = consumer.clone();
            move || {
                let mut items = Vec::new();
                while let Some(item) = consumer.pop() {
                    items.push(item);
                }
                items
            }
        });

        producer_thread.join().unwrap();
        let items1 = consumer_thread1.join().unwrap();
        let items2 = consumer_thread2.join().unwrap();

        assert_eq!(items1.len() + items2.len(), 10);
        assert_eq!(
            [items1, items2]
                .concat()
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            (0..10).collect::<std::collections::HashSet<_>>()
        );
    }
}
