//! Bounded work-pull pool over `std::thread` (issue 20, I20-mech).
//!
//! [`run_bounded`] runs a shared closure over a slice with at most
//! `concurrency` in-flight items, reassembling results in input order. The
//! only synchronization primitive is an `AtomicUsize` work counter; per-item
//! results are stored by index and unwrapped after the scope joins. A worker
//! panic propagates out of `scope` exactly as a panic in a sequential loop
//! would (no swallowing).

/// Run `f` over every item of `items` with at most `concurrency` in-flight
/// calls, returning results in input order (issue 20, I20-mech/I20-one).
///
/// `concurrency <= 1` takes a dedicated sequential path: `f` runs on the
/// caller's thread, byte-for-byte today's loop behavior - no threads are
/// spawned (I20-one). Otherwise workers = `min(concurrency, items.len())`
/// each `fetch_add` the next item index off an `AtomicUsize` and store the
/// per-item result in its pre-sized slot; the report is assembled in input
/// order after the scope joins. A worker panic propagates out of `scope`
/// (the first panic, after all workers are joined) exactly as a panic in a
/// sequential loop would.
pub(crate) fn run_bounded<T, R>(concurrency: u32, items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    if concurrency <= 1 || items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let workers = std::cmp::min(concurrency as usize, items.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<R>>> = (0..items.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if idx >= items.len() {
                        break;
                    }
                    let result = f(&items[idx]);
                    *slots[idx].lock().unwrap() = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap().expect("slot filled"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bounded_caps_in_flight() {
        // I20-mech: over 32 items at concurrency 4 the max-concurrent gauge
        // never exceeds 4, and every index is processed exactly once.
        let in_flight = std::sync::atomic::AtomicUsize::new(0);
        let max_in_flight = std::sync::atomic::AtomicUsize::new(0);
        let processed = std::sync::Mutex::new(Vec::new());
        let items: Vec<usize> = (0..32).collect();
        let f = |&i: &usize| {
            let cur = in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_in_flight.fetch_max(cur, std::sync::atomic::Ordering::SeqCst);
            std::thread::yield_now();
            processed.lock().unwrap().push(i);
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        };
        run_bounded(4, &items, f);
        assert!(
            max_in_flight.load(std::sync::atomic::Ordering::SeqCst) <= 4,
            "in-flight gauge must never exceed the concurrency cap"
        );
        let mut seen = processed.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, items, "every index must be processed exactly once");
    }

    #[test]
    fn run_bounded_results_in_input_order() {
        // I20-mech: the closure sleeps inversely to the index (adversarial
        // reverse completion); results must come back in input order.
        let items: Vec<usize> = (0..16).collect();
        let results = run_bounded(4, &items, |&i| {
            std::thread::sleep(std::time::Duration::from_millis(16 - i as u64));
            i
        });
        assert_eq!(results, items);
    }

    #[test]
    fn run_bounded_concurrency_1_runs_on_caller_thread() {
        // I20-one: `concurrency = 1` takes the dedicated sequential path -
        // every item executes on the caller's thread (no threads spawned).
        let caller = std::thread::current().id();
        let items: Vec<usize> = (0..8).collect();
        let results = run_bounded(1, &items, |&_i| std::thread::current().id());
        assert!(
            results.iter().all(|&id| id == caller),
            "concurrency = 1 must run on the caller thread"
        );
    }

    #[test]
    fn run_bounded_error_isolation() {
        // I20-mech: per-item `Result`s are returned individually; one `Err`
        // does not drop neighbors or reorder the vector.
        let items: Vec<usize> = (0..8).collect();
        let results = run_bounded(4, &items, |&i| -> Result<usize, String> {
            if i == 3 {
                Err("boom".to_string())
            } else {
                Ok(i)
            }
        });
        assert_eq!(results.len(), items.len());
        for (i, r) in results.iter().enumerate() {
            if i == 3 {
                assert_eq!(r.as_ref().unwrap_err(), "boom");
            } else {
                assert_eq!(r.as_ref().unwrap(), &i);
            }
        }
    }
}
