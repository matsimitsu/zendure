//! Tests for `scan.rs`.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::battery::BatteryState;
use crate::config::Config;
use crate::device::BatteryTelemetry;
use crate::journal::testing::nowhere;
use crate::units::Watts;

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

fn slot() -> Slot<Sample> {
    Slot::new(WINDOW, epoch())
}

/// One instant every test below measures its timeline from, so an assertion
/// is about the offsets relative to each other, never about when the test
/// happened to run.
fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Past the blind window, measured from `epoch`.
fn aged_out() -> Instant {
    epoch() + WINDOW + Duration::from_millis(1)
}

/// The number every operator line reports: consecutive, and reset by a
/// sample rather than by the round that carried it. Nothing stands a source
/// down on it, so a count that never reset would only ever misreport.
#[test]
fn a_sample_resets_the_consecutive_failure_count() {
    let slot = slot();

    assert_eq!(slot.fail().get(), 1);
    assert_eq!(slot.fail().get(), 2);

    assert_eq!(
        slot.deliver(ROUND, Clock::test_at(1_000), epoch(), Sample)
            .get(),
        2,
        "a delivery reports what the source had failed before it"
    );
    assert_eq!(slot.fail().get(), 1);
}

/// The self-recovery requirement: nothing supervises a sampler, so a source
/// that starts answering again has to come back on its own.
#[test]
fn one_good_sample_brings_a_down_source_back() {
    let slot = slot();

    assert!(slot.take(ROUND, aged_out()).down().is_some());

    slot.deliver(ROUND, Clock::test_at(1_000), aged_out(), Sample);
    assert!(matches!(
        slot.take(ROUND, aged_out()),
        Delivery::Fresh(_, _)
    ));
}

/// A source that never comes up at all must still stand the loop down, rather
/// than staying quiet forever behind a failure count that never rises.
#[test]
fn a_slot_that_was_never_sampled_ages_out() {
    let slot = slot();

    assert!(matches!(slot.take(ROUND, epoch()), Delivery::Quiet));

    let failures = slot
        .take(ROUND, aged_out())
        .down()
        .expect("a slot older than the blind window is down");
    assert_eq!(failures.get(), 0, "nothing failed — it never answered");
}

/// HKJMQB1: a forward NTP step (no RTC correcting the wall clock at boot)
/// stamps a delivery arbitrarily far in the future. Staleness ages on the
/// monotonic clock, not that stamp, so a healthy source must not look down.
#[test]
fn a_forward_wall_clock_jump_does_not_falsely_age_out_a_healthy_source() {
    let slot = slot();

    slot.deliver(ROUND, Clock::test_at(i64::MAX / 2), epoch(), Sample);

    assert!(
        matches!(slot.take(ROUND, epoch()), Delivery::Fresh(_, _)),
        "a delivery stamped with a wall clock far in the future must not \
         itself look aged out on the monotonic clock the slot actually ages on"
    );
}

/// The mirror case: a backward step (an RTC correcting after boot) stamps a
/// delivery with a wall clock behind the slot's own opening. The monotonic
/// age bound must still trip once real time has actually moved past the
/// blind window — a step must not disable it until the wall clock catches up.
#[test]
fn a_backward_wall_clock_jump_does_not_disable_the_age_bound() {
    let slot = slot();

    slot.deliver(ROUND, Clock::test_at(-1), epoch(), Sample);

    assert!(
        slot.take(ROUND, aged_out()).down().is_some(),
        "a wall clock stamped in the past must not stop the monotonic age \
         bound from tripping once real time has moved past the blind window"
    );
}

/// The hole a pure failure count leaves: a request that never returns never
/// reports a failure, so five rounds of asking leave one request outstanding,
/// nothing delivered and nothing failed — and only the age bound catches it.
#[tokio::test]
async fn a_source_that_hangs_goes_down_without_ever_failing() {
    struct Hanging {
        entered: AtomicU64,
    }

    impl Sampler for Hanging {
        type Reading = Sample;

        fn id(&self) -> &str {
            "hanging"
        }

        async fn sample(&self) -> Result<Sample, PollError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    let source = Arc::new(Hanging {
        entered: AtomicU64::new(0),
    });
    let slot = Arc::new(slot());
    let (requests, _) = watch::channel(TickSeq::FIRST);
    let task = tokio::spawn(sample_loop(
        source.clone(),
        slot.clone(),
        requests.subscribe(),
        Arc::new(nowhere()),
        chrono_tz::UTC,
    ));

    for _ in 0..5 {
        requests.send_modify(|seq| *seq = seq.next());
        tokio::task::yield_now().await;
    }

    assert_eq!(
        source.entered.load(Ordering::SeqCst),
        1,
        "the source has to be in a request that never returned"
    );
    assert!(
        matches!(slot.take(ROUND, epoch()), Delivery::Quiet),
        "a hung request delivers nothing and fails nothing"
    );

    let failures = slot
        .take(ROUND, aged_out())
        .down()
        .expect("a source that never answers is down");
    assert_eq!(failures.get(), 0);

    task.abort();
}

/// Take-once: a second read of the same sample would date a later decision's
/// inputs differently from each other, and would integrate the same joules
/// into the round-trip-efficiency window twice.
#[test]
fn a_fresh_sample_is_taken_exactly_once() {
    let slot = slot();

    slot.deliver(ROUND, Clock::test_at(1_000), epoch(), Sample);
    assert!(matches!(slot.take(ROUND, epoch()), Delivery::Fresh(_, _)));
    assert!(matches!(slot.take(ROUND, epoch()), Delivery::Quiet));
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
    let inbox = Inbox::new(ids.map(DeviceId::new), epoch(), WINDOW);

    for id in ids {
        inbox
            .device_slot(&DeviceId::new(id))
            .expect("every id was registered")
            .deliver(ROUND, Clock::test_at(1_000), epoch(), battery_reading());
    }

    let taken = inbox.take_devices(ROUND, epoch());
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
    let inbox = Inbox::new(ids.map(DeviceId::new), epoch(), WINDOW);
    let round = TickSeq::FIRST.next();

    inbox
        .device_slot(&DeviceId::new("early"))
        .expect("every id was registered")
        .deliver(
            TickSeq::FIRST,
            Clock::test_at(0),
            epoch(),
            battery_reading(),
        );
    inbox
        .device_slot(&DeviceId::new("ontime"))
        .expect("every id was registered")
        .deliver(round, Clock::test_at(3_000), epoch(), battery_reading());

    let taken: Vec<_> = inbox
        .take_devices(round, epoch())
        .into_iter()
        .map(|(id, delivery)| (id.to_string(), matches!(delivery, Delivery::Fresh(_, _))))
        .collect();

    assert_eq!(
        taken,
        [("early".to_string(), false), ("ontime".to_string(), true)]
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
        Arc::new(slot()),
        requests.subscribe(),
        Arc::new(nowhere()),
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
