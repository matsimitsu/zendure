use super::*;
use crate::units::Setpoint;

fn pack(json: &str) -> PackData {
    serde_json::from_str(json).expect("pack fixture should parse")
}

/// A client pointed at a port nothing listens on, so any write it does
/// attempt fails fast instead of reaching a device — the real transport, for
/// the tests that are actually about what a genuine failure looks like.
fn client() -> ZendureClient {
    ZendureClient::new("127.0.0.1:1", "TESTSN".to_string(), POLL_INTERVAL_FLOOR)
}

/// A genuine `ZendureError::Status`, built from a real HTTP status line
/// without a socket: `http::Response` converts into a `reqwest::Response`,
/// and `error_for_status` does the rest. Replaces hand-crafting a raw HTTP
/// response over a real listener just to get a status line past it.
fn status_error(code: u16) -> ZendureError {
    let response: reqwest::Response = http::Response::builder()
        .status(code)
        .body(String::new())
        .expect("a bare status line is a valid response")
        .into();
    response
        .error_for_status()
        .expect_err("this status is always outside the 2xx range")
        .into()
}

/// `parse_report` is the one place every poll passes through, and it must
/// actually feed the report's `smartMode` to the ledger — `mode.rs` and
/// `ledger.rs` each test their own half of this in isolation, so this is the
/// one test proving the wiring between them.
#[test]
fn a_parsed_report_updates_the_tracked_storage_mode() {
    let client = client();

    client
        .parse_report(r#"{"properties":{"smartMode":0}}"#.to_string())
        .expect("fixture should parse");

    assert_eq!(client.ledger.tracked_storage_mode(), StorageMode::Flash);
}

/// A charge out of standby still goes through `ensure_ram_mode`: the write
/// is attempted (and fails, against a closed port) rather than skipped, and
/// nothing about the failed attempt is recorded as having landed.
#[tokio::test]
async fn a_charge_after_standby_still_wakes_the_device_and_writes() {
    let client = client();
    client.ledger.record_idle();
    client
        .parse_report(r#"{"properties":{"smartMode":0}}"#.to_string())
        .expect("fixture should parse");

    client
        .apply_command(&Command::SetCharge(Setpoint::new(500)))
        .await
        .expect_err("nothing is listening on port 1");

    assert_eq!(client.ledger.tracked_storage_mode(), StorageMode::Flash);
    assert_eq!(
        client.ledger.tracked_state().input_limit,
        Some(Setpoint::ZERO)
    );
}

/// Tracked state is what landed, not what was attempted: a standby whose
/// POST failed left the device where it was, and claiming otherwise would
/// suppress the retry forever.
#[tokio::test]
async fn a_failed_standby_write_does_not_claim_the_device_is_in_standby() {
    let client = client();

    client
        .apply_command(&Command::SetStandby)
        .await
        .expect_err("nothing is listening on port 1");

    assert_eq!(client.ledger.tracked_storage_mode(), StorageMode::Ram);
    assert!(needs_write(
        &Command::SetStandby,
        client.ledger.tracked_state()
    ));
}

/// A failed charge must not claim the `acMode` it was attempting to set —
/// the same rule `record_input_limit`'s doc already states, now also true
/// of `record_ac_mode`.
#[tokio::test]
async fn a_failed_charge_does_not_record_the_ac_mode_it_attempted() {
    let client = client();

    client
        .apply_command(&Command::SetCharge(Setpoint::new(500)))
        .await
        .expect_err("nothing is listening on port 1");

    assert_eq!(client.ledger.tracked_ac_mode(), None);
}

/// Never reaching the device and being refused by it are different
/// failures; a caller reading `ZendureError` should be able to tell them
/// apart rather than pattern-matching a message string.
#[tokio::test]
async fn a_closed_port_is_a_transport_error() {
    let client = client();

    let err = client
        .get_properties_raw()
        .await
        .expect_err("nothing is listening on port 1");

    assert!(matches!(err, ZendureError::Transport(_)));
}

/// The bug this ticket exists for: a well-formed body riding on a 500 used
/// to parse straight through as success, because nothing checked the status
/// before decoding. `error_for_status` must reject it on the status alone.
#[tokio::test]
async fn a_bad_status_on_read_is_an_error_even_with_a_well_formed_body() {
    let (client, fake) = ZendureClient::fake("TESTSN");
    fake.queue_get(Err(status_error(500)));

    let err = client
        .get_properties_raw()
        .await
        .expect_err("a 500 must not read as success");

    assert!(matches!(err, ZendureError::Status(_)));
}

/// The same bug on the write side: a 500 response to a write used to be
/// recorded as landed, since a POST that reaches the device at all returns
/// `Ok` regardless of status.
#[tokio::test]
async fn a_bad_status_on_write_is_an_error_and_nothing_is_recorded_as_landed() {
    let (client, fake) = ZendureClient::fake("TESTSN");
    fake.queue_post(Err(status_error(500)));

    let err = client
        .apply_command(&Command::SetCharge(Setpoint::new(500)))
        .await
        .expect_err("a 500 must not read as a landed write");

    assert!(matches!(err, ZendureError::Status(_)));
    assert_eq!(client.ledger.tracked_state().input_limit, None);
    assert_eq!(client.ledger.tracked_ac_mode(), None);
}

/// A 200 whose body this build cannot decode is a parse failure, not a
/// silent success — `get_properties` shares `get_properties_raw`'s status
/// check and adds its own decode on top.
#[tokio::test]
async fn an_undecodable_body_on_get_properties_is_a_parse_error() {
    let (client, fake) = ZendureClient::fake("TESTSN");
    fake.queue_get(Ok("not json".to_string()));

    let err = client.get_properties().await.expect_err("not valid JSON");

    assert!(matches!(err, ZendureError::Parse(_)));
}

/// The other half of `a_failed_charge_does_not_record_the_ac_mode_it_attempted`:
/// a charge that actually lands does record the mode it sent, once the
/// write is confirmed rather than before.
#[tokio::test]
async fn a_landed_charge_records_the_ac_mode_only_after_the_write_succeeds() {
    let (client, fake) = ZendureClient::fake("TESTSN");
    fake.queue_post(Ok(()));
    assert_eq!(client.ledger.tracked_ac_mode(), None);

    client
        .apply_command(&Command::SetCharge(Setpoint::new(500)))
        .await
        .expect("the fake answered the write with success");

    assert_eq!(client.ledger.tracked_ac_mode(), Some(AcMode::Charge));
    assert_eq!(
        client.ledger.tracked_state().input_limit,
        Some(Setpoint::new(500))
    );
}

#[test]
fn the_pack_type_table_knows_the_packs_this_build_ships_with() {
    assert_eq!(known_pack_type_capacity(500), Some(WattHours(2400.0)));
    assert_eq!(known_pack_type_capacity(501), Some(WattHours(1920.0)));
}

/// The case this exists for: an expansion pack newer than the table. An
/// AB3000 holds 2880 Wh, so until its `packType` is added here the total is
/// a guess — one the controller keeps running on, but never silently.
#[test]
fn an_unrecognised_pack_type_is_not_in_the_table() {
    assert_eq!(known_pack_type_capacity(999), None);
}

/// Loud and running beats silent and stopped: an unidentified pack still
/// yields a capacity, so the dashboard and `usable_kwh` keep working
/// (pessimistically) rather than going blank.
#[test]
fn an_unidentified_pack_still_reports_a_capacity() {
    assert_eq!(
        pack_capacity(&pack(r#"{"packType":999}"#)),
        UNIDENTIFIED_PACK_CAPACITY
    );
}

/// A pack that reports no `packType` at all used to take the 1920 Wh
/// default with nothing said about it — the same guess as an unrecognised
/// type, and it deserves the same warning.
#[test]
fn a_pack_with_no_pack_type_is_also_unidentified() {
    assert_eq!(
        pack_capacity(&pack(r#"{"sn":"JO4AENCN4900105"}"#)),
        UNIDENTIFIED_PACK_CAPACITY
    );
}

/// The shape the AB3000 arrives in: a second pack alongside the built-in
/// one, each mapped on its own type rather than the first pack's standing
/// for both.
#[test]
fn each_pack_is_mapped_on_its_own_type() {
    let packs = [pack(r#"{"packType":500}"#), pack(r#"{"packType":501}"#)];

    assert_eq!(
        pack_capacities(&packs),
        vec![WattHours(2400.0), WattHours(1920.0)]
    );
}

/// The property `PollError` exists for: a response we failed to decode is
/// the one most worth having on record, so it must carry the exact bytes
/// that failed to parse rather than discarding them alongside the error.
#[test]
fn a_parse_failure_carries_the_raw_body() {
    let body = "not valid json".to_string();

    let err = client()
        .parse_report(body.clone())
        .expect_err("not valid JSON");

    assert_eq!(
        err.raw,
        Some(RawCapture {
            kind: "zendure_poll",
            body,
        }),
    );
    assert!(err.error.contains("parse error"));
}

/// A report with no `packData` at all leaves `pack_capacities` as `None`
/// rather than `Some(vec![])` — the caller's cue to keep its last known
/// set instead of publishing a capacity of zero.
#[test]
fn a_report_with_no_pack_data_reports_no_pack_capacities() {
    let report: ZendureReport = serde_json::from_str(r#"{"properties":{}}"#).unwrap();

    let reading = reading_from_report(&report, None, &AC2400_PLUS);

    assert_eq!(reading.telemetry.pack_capacities, None);
    assert!(reading.telemetry.pack_temps.is_empty());
}

/// `packData` matching the device's own `packNum` count is trusted and
/// summed in full.
#[test]
fn pack_data_matching_pack_num_reports_full_capacity() {
    let report: ZendureReport = serde_json::from_str(
        r#"{"properties":{"packNum":2},"packData":[{"packType":500},{"packType":501}]}"#,
    )
    .unwrap();

    let reading = reading_from_report(&report, None, &AC2400_PLUS);

    assert_eq!(
        reading.telemetry.pack_capacities,
        Some(vec![WattHours(2400.0), WattHours(1920.0)])
    );
}

/// `packData` shorter than the device's own `packNum` count is a pack
/// that hasn't reported in yet, not the true total — treated the same as
/// no pack data so the caller keeps its last known capacity instead of
/// publishing an undersized figure.
#[test]
fn pack_data_short_of_pack_num_reports_no_pack_capacities() {
    let report: ZendureReport =
        serde_json::from_str(r#"{"properties":{"packNum":2},"packData":[{"packType":500}]}"#)
            .unwrap();

    let reading = reading_from_report(&report, None, &AC2400_PLUS);

    assert_eq!(reading.telemetry.pack_capacities, None);
}

/// When the device omits `packNum` entirely, `packData` is trusted as-is
/// rather than rejected for lack of a count to check it against.
#[test]
fn pack_data_without_pack_num_is_trusted_as_is() {
    let report: ZendureReport =
        serde_json::from_str(r#"{"properties":{},"packData":[{"packType":500}]}"#).unwrap();

    let reading = reading_from_report(&report, None, &AC2400_PLUS);

    assert_eq!(
        reading.telemetry.pack_capacities,
        Some(vec![WattHours(2400.0)])
    );
}
