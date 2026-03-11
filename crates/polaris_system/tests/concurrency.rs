//! Concurrent access tests for `polaris_system`.
//!
//! These tests verify thread-safety and concurrent access patterns.

use polaris_system::resource::{LocalResource, Resources};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

// Test resource types
#[derive(Debug, PartialEq, Clone)]
struct Counter {
    value: i32,
}

impl LocalResource for Counter {}

#[derive(Debug, PartialEq, Clone)]
struct Config {
    name: String,
}

impl LocalResource for Config {}

/// Test concurrent reads from multiple threads.
#[test]
fn concurrent_reads_from_multiple_threads() {
    let mut resources = Resources::new();
    resources.insert(Counter { value: 42 });

    // Wrap in Arc for thread sharing
    let resources = Arc::new(resources);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let resources = Arc::clone(&resources);
            thread::spawn(move || {
                // Multiple concurrent reads should all succeed
                for _ in 0..100 {
                    let counter = resources.get::<Counter>().unwrap();
                    assert_eq!(counter.value, 42);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

/// Test that read succeeds while another thread holds read.
#[test]
fn multiple_concurrent_readers_allowed() {
    let mut resources = Resources::new();
    resources.insert(Counter { value: 100 });

    let resources = Arc::new(resources);

    // Spawn threads that hold reads for a while
    let handles: Vec<_> = (0..3)
        .map(|i| {
            let resources = Arc::clone(&resources);
            thread::spawn(move || {
                let counter = resources.get::<Counter>().unwrap();
                // Hold the read for a bit
                thread::sleep(Duration::from_millis(10));
                assert_eq!(counter.value, 100);
                i
            })
        })
        .collect();

    // All should succeed
    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

/// Test read-write contention returns `BorrowConflict`.
#[test]
fn read_write_contention_returns_error() {
    let mut resources = Resources::new();
    resources.insert(Counter { value: 0 });

    let resources = Arc::new(resources);
    let barrier = Arc::new(Barrier::new(2));
    let write_acquired = Arc::new(AtomicBool::new(false));

    // Thread 1: Acquire write lock and hold it
    let resources1 = Arc::clone(&resources);
    let barrier1 = Arc::clone(&barrier);
    let write_acquired1 = Arc::clone(&write_acquired);

    let writer = thread::spawn(move || {
        let _guard = resources1.get_mut::<Counter>().unwrap();
        write_acquired1.store(true, Ordering::SeqCst);
        barrier1.wait(); // Signal that write is held
        thread::sleep(Duration::from_millis(50));
        // Write lock released when guard drops
    });

    // Thread 2: Try to read while write is held
    let resources2 = Arc::clone(&resources);
    let barrier2 = Arc::clone(&barrier);
    let write_acquired2 = Arc::clone(&write_acquired);

    let reader = thread::spawn(move || {
        barrier2.wait(); // Wait for writer to acquire lock
        assert!(write_acquired2.load(Ordering::SeqCst));

        // Read should fail with BorrowConflict while write is held
        let result = resources2.get::<Counter>();
        assert!(result.is_err());
    });

    writer.join().expect("Writer thread panicked");
    reader.join().expect("Reader thread panicked");
}

/// Test that different resource types can be accessed concurrently.
#[test]
fn different_resource_types_no_contention() {
    let mut resources = Resources::new();
    resources.insert(Counter { value: 1 });
    resources.insert(Config {
        name: "test".into(),
    });

    let resources = Arc::new(resources);

    // One thread modifies Counter, another reads Config
    let resources1 = Arc::clone(&resources);
    let resources2 = Arc::clone(&resources);

    let counter_thread = thread::spawn(move || {
        let mut counter = resources1.get_mut::<Counter>().unwrap();
        thread::sleep(Duration::from_millis(10));
        counter.value += 1;
    });

    let config_thread = thread::spawn(move || {
        // This should succeed - different resource type
        let config = resources2.get::<Config>().unwrap();
        assert_eq!(config.name, "test");
    });

    counter_thread.join().expect("Counter thread panicked");
    config_thread.join().expect("Config thread panicked");
}
