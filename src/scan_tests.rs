//! Tests for `scan.rs`.

use super::*;
use crate::battery::BatteryState;
use crate::config::Config;
use crate::device::BatteryTelemetry;
use crate::units::{RetentionDays, Watts};

/// Catches a timeout that would leave a request in flight when the next round
/// starts, and one that would hold a sampler past the fixed ceiling however
/// long the period grows. The domain reaches down to the shortest period a
/// virtual device can be given, since the floor does not apply there.
#[test]
fn a_request_gives_up_before_the_next_poll_and_within_a_fixed_bound() {
    let periods = [
        50, 250, 500, 1_000, 2_000, 2_500, 3_000, 4_000, 5_000, 9_000,
    ]
    .into_iter()
    .map(Duration::from_millis)
    .chain(
        [10, 11, 60, 150, 3_600, 86_400]
            .into_iter()
            .map(Duration::from_secs),
    );

    for period in periods {
        let timeout = request_timeout(period);

        assert!(timeout < period, "{period:?} derived {timeout:?}");
        assert!(
            timeout <= MAX_REQUEST_TIMEOUT,
            "{period:?} derived {timeout:?}"
        );
    }
}

/// Catches a change to the timeout the deployed period derives, which is the
/// one that reaches the box.
#[test]
fn the_shipped_scan_period_derives_a_two_second_timeout() {
    let (config, _) = Config::from_toml_str(include_str!("../config.example.toml"))
        .expect("the example config parses");

    assert_eq!(
        request_timeout(config.device.poll_interval()),
        Duration::from_secs(2)
    );
}

/// How long the loop may go without a usable reading before it stands down.
const WINDOW: Duration = Duration::from_secs(60);

/// The round a test asks for and answers, where it only needs one.
const ROUND: TickSeq = TickSeq::FIRST;

/// A reading with no wire format behind it, which is all these tests need.
struct Sample;

impl Captured for Sample {
    fn raw(&self) -> Option<&RawCapture> {
        None
    }
}

fn slot(tolerance: u32) -> Slot<Sample> {
    Slot::new(FailureTolerance(tolerance), WINDOW, at(0))
}

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

/// Past the blind window, measured from the inbox opening.
fn aged_out() -> Timestamp {
    at(Elapsed::of(WINDOW).as_millis() + 1)
}

/// A journal that keeps nothing and — crucially for the paused-clock tests
/// below — starts no `spawn_blocking` writer.
fn nowhere_journal() -> Arc<Journal> {
    let (journal, writer) = Journal::open(
        std::path::Path::new("/dev/null/journal.db"),
        RetentionDays::new(1).expect("one day is a valid retention"),
        "0.0.0-test",
        &(),
    );
    assert!(writer.is_none(), "this journal must have no writer task");
    Arc::new(journal)
}

/// An intermittent failure costs the round it happened in and nothing else.
#[test]
fn a_source_that_fails_once_then_succeeds_stays_healthy() {
    let slot = slot(3);

    slot.fail();
    assert!(matches!(slot.take(ROUND, at(1_000)), Delivery::Cold));

    slot.deliver(ROUND, Clock::test_at(2_000), Sample);
    assert!(matches!(slot.take(ROUND, at(2_000)), Delivery::Fresh(_, _)));
}

#[test]
fn failing_past_the_tolerance_takes_a_source_down() {
    let slot = slot(3);

    for _ in 0..3 {
        slot.fail();
    }
    assert!(
        !slot.take(ROUND, at(0)).is_down(),
        "three failures is inside a tolerance of three"
    );

    slot.fail();
    let Delivery::Down(failures) = slot.take(ROUND, at(0)) else {
        panic!("a fourth failure must exceed a tolerance of three")
    };
    assert_eq!(failures.get(), 4);
}

/// The self-recovery requirement: nothing supervises a sampler, so a source
/// that starts answering again has to come back on its own.
#[test]
fn one_good_sample_brings_a_down_source_back() {
    let slot = slot(2);

    for _ in 0..5 {
        slot.fail();
    }
    assert!(slot.take(ROUND, at(0)).is_down());

    slot.deliver(ROUND, Clock::test_at(1_000), Sample);
    assert!(matches!(slot.take(ROUND, at(1_000)), Delivery::Fresh(_, _)));
}

/// A source that never comes up at all must still stand the loop down, rather
/// than staying quiet forever behind a failure count that never rises.
#[test]
fn a_slot_that_was_never_sampled_ages_out() {
    let slot = slot(100);

    assert!(matches!(slot.take(ROUND, at(0)), Delivery::Cold));

    let Delivery::Down(failures) = slot.take(ROUND, aged_out()) else {
        panic!("a slot older than the blind window is down")
    };
    assert_eq!(failures.get(), 0, "nothing failed — it never answered");
}

/// The hole a pure failure count leaves: a request that never returns never
/// reports a failure, so only the age bound catches it.
#[tokio::test]
async fn a_source_that_hangs_goes_down_without_ever_failing() {
    struct Hanging;

    impl Sampler for Hanging {
        type Reading = Sample;

        fn id(&self) -> &str {
            "hanging"
        }

        async fn sample(&self) -> Result<Sample, PollError> {
            std::future::pending().await
        }
    }

    let slot = Arc::new(slot(100));
    let (requests, _) = watch::channel(TickSeq::FIRST);
    let task = tokio::spawn(sample_loop(
        Arc::new(Hanging),
        slot.clone(),
        requests.subscribe(),
        nowhere_journal(),
        chrono_tz::UTC,
    ));

    for _ in 0..5 {
        requests.send_modify(|seq| *seq = seq.next());
        tokio::task::yield_now().await;
    }

    let Delivery::Down(failures) = slot.take(ROUND, aged_out()) else {
        panic!("a source that never answers is down")
    };
    assert_eq!(failures.get(), 0);

    task.abort();
}

/// Take-once: a second read of the same sample would date a later decision's
/// inputs differently from each other, and would integrate the same joules
/// into the round-trip-efficiency window twice.
#[test]
fn a_fresh_sample_is_taken_exactly_once() {
    let slot = slot(3);

    slot.deliver(ROUND, Clock::test_at(1_000), Sample);
    assert!(matches!(slot.take(ROUND, at(1_000)), Delivery::Fresh(_, _)));
    assert!(matches!(slot.take(ROUND, at(1_000)), Delivery::Quiet));
}

fn battery_reading() -> BatteryReading {
    BatteryReading {
        state: BatteryState::test_sample(),
        telemetry: BatteryTelemetry {
            charge: Watts::ZERO,
            discharge: Watts::ZERO,
            pack_capacities: None,
            pack_temps: Vec::new(),
            enclosure_temp: None,
            min_soc: None,
        },
        raw: None,
    }
}

/// A fleet must fold the same way twice running, whichever box answered
/// first.
#[test]
fn devices_are_taken_in_id_order_whatever_order_they_answered() {
    let ids = ["zzz", "aaa", "mmm"];
    let inbox = Inbox::new(
        FailureTolerance(3),
        ids.map(|id| (DeviceId::new(id), FailureTolerance(3))),
        at(0),
        WINDOW,
    );

    for id in ids {
        inbox
            .device_slot(&DeviceId::new(id))
            .expect("every id was registered")
            .deliver(ROUND, Clock::test_at(1_000), battery_reading());
    }

    let taken = inbox.take_devices(ROUND, at(1_000));
    let order: Vec<String> = taken.iter().map(|(id, _)| id.to_string()).collect();

    assert_eq!(order, ["aaa", "mmm", "zzz"]);
    assert!(
        taken
            .iter()
            .all(|(_, delivery)| matches!(delivery, Delivery::Fresh(_, _)))
    );
}

/// The matched-age invariant at its narrowest: a sample that arrived too late
/// for its round must not be served beside one taken for the round that just
/// completed, or the two terms a setpoint subtracts are a tick apart.
#[test]
fn a_sample_stamped_with_an_earlier_round_is_not_fresh_for_this_one() {
    let ids = ["early", "ontime"];
    let inbox = Inbox::new(
        FailureTolerance(100),
        ids.map(|id| (DeviceId::new(id), FailureTolerance(100))),
        at(0),
        WINDOW,
    );
    let round = TickSeq::FIRST.next();

    inbox
        .device_slot(&DeviceId::new("early"))
        .expect("every id was registered")
        .deliver(TickSeq::FIRST, Clock::test_at(0), battery_reading());
    inbox
        .device_slot(&DeviceId::new("ontime"))
        .expect("every id was registered")
        .deliver(round, Clock::test_at(3_000), battery_reading());

    let taken: Vec<_> = inbox
        .take_devices(round, at(3_000))
        .into_iter()
        .map(|(id, delivery)| (id.to_string(), matches!(delivery, Delivery::Fresh(_, _))))
        .collect();

    assert_eq!(
        taken,
        [("early".to_string(), false), ("ontime".to_string(), true)]
    );
}

#[test]
fn the_tolerance_is_the_rounds_that_fit_the_blind_window() {
    assert_eq!(
        FailureTolerance::over(Duration::from_secs(60), Duration::from_secs(3)).get(),
        20
    );
    assert_eq!(
        FailureTolerance::over(Duration::from_secs(60), Duration::from_secs(150)).get(),
        1,
        "a period longer than the window must not stand the fleet down on one failure"
    );
    assert_eq!(
        FailureTolerance::over(Duration::ZERO, Duration::from_secs(3)).get(),
        1
    );
}

/// One in-flight request per source, and a requester that never waits for it:
/// the watermark stays at one while rounds pile up, and the ticker keeps its
/// own period.
#[tokio::test(start_paused = true)]
async fn a_slow_sample_neither_delays_nor_queues_behind_the_requester() {
    const PERIOD: Duration = Duration::from_millis(100);
    const ROUNDS: u32 = 20;

    struct Slow {
        in_flight: AtomicU64,
        watermark: AtomicU64,
    }

    impl Sampler for Slow {
        type Reading = Sample;

        fn id(&self) -> &str {
            "slow"
        }

        async fn sample(&self) -> Result<Sample, PollError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.watermark.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(PERIOD * 10).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Sample)
        }
    }

    let source = Arc::new(Slow {
        in_flight: AtomicU64::new(0),
        watermark: AtomicU64::new(0),
    });
    let (requests, _) = watch::channel(TickSeq::FIRST);
    let task = tokio::spawn(sample_loop(
        source.clone(),
        Arc::new(slot(100)),
        requests.subscribe(),
        nowhere_journal(),
        chrono_tz::UTC,
    ));

    let started = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(PERIOD);
    ticker.tick().await;
    for _ in 0..ROUNDS {
        ticker.tick().await;
        requests.send_modify(|seq| *seq = seq.next());
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < PERIOD * (ROUNDS + 2),
        "the requester kept its own period: {elapsed:?}"
    );
    assert_eq!(
        source.watermark.load(Ordering::SeqCst),
        1,
        "rounds missed while a sample was in flight must coalesce, not queue"
    );

    task.abort();
}
