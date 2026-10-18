use std::collections::BTreeMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// What the register map documents a poll to find, one field per argument
/// so each fixture names only what it is about.
struct Registers {
    status: u16,
    cable: u16,
    phases: u16,
    currents: [u16; 3],
    voltages: [u16; 3],
    power: u32,
    total_energy: u32,
    session_energy: u32,
    session_seconds: u32,
}

const CHARGING: Registers = Registers {
    status: 2,
    cable: 3,
    phases: 1,
    currents: [15_900, 16_000, 16_100],
    voltages: [229, 231, 233],
    power: 11_040,
    total_energy: 12_345,
    session_energy: 4_200,
    session_seconds: 4_140,
};

const IDLE: Registers = Registers {
    status: 0,
    cable: 0,
    phases: 1,
    currents: [0; 3],
    voltages: [230; 3],
    power: 0,
    total_energy: 12_345,
    session_energy: 0,
    session_seconds: 0,
};

const SUSPENDED: Registers = Registers {
    status: 4,
    cable: 3,
    phases: 0,
    currents: [0; 3],
    voltages: [230; 3],
    power: 0,
    total_energy: 12_387,
    session_energy: 20_000,
    session_seconds: 9_000,
};

fn split(value: u32) -> [u16; 2] {
    let [a, b, c, d] = value.to_be_bytes();
    [u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d])]
}

/// The register map as a device would hold it, one word per address.
fn device_map(r: &Registers) -> BTreeMap<u16, u16> {
    let mut map = BTreeMap::new();
    map.insert(PHASES, r.phases);
    map.insert(CHARGEPOINT_STATE, r.status);
    map.insert(1001, u16::from(r.status == 2));
    map.insert(CABLE_STATE, r.cable);
    for (register, value) in CURRENTS.iter().zip(r.currents) {
        map.insert(*register, value);
    }
    for (register, value) in VOLTAGES.iter().zip(r.voltages) {
        map.insert(*register, value);
    }
    for (start, value) in [
        (POWER, r.power),
        (TOTAL_ENERGY, r.total_energy),
        (SESSION_ENERGY, r.session_energy),
        (SESSION_DURATION, r.session_seconds),
    ] {
        let [high, low] = split(value);
        map.insert(start, high);
        map.insert(start + 1, low);
    }
    map
}

/// The blocks a poll would read off that map, zero where the map is silent.
fn blocks(r: &Registers) -> RegisterBlocks {
    let map = device_map(r);
    let mut blocks = RegisterBlocks::default();
    for span in REQUIRED_SPANS.into_iter().chain(BEST_EFFORT_SPANS) {
        let words = (span.start..span.start + span.count)
            .map(|register| map.get(&register).copied().unwrap_or(0))
            .collect();
        blocks.insert(span.start, words);
    }
    blocks
}

#[test]
fn a_charging_car_parses_from_the_documented_map() {
    let state = parse(&blocks(&CHARGING)).unwrap();

    assert_eq!(
        state,
        ChargerState {
            status: ChargePointStatus::Charging,
            cable: CableState::VehicleLocked,
            power: ChargerDraw::new(11_040),
            currents: [MilliAmps(15_900), MilliAmps(16_000), MilliAmps(16_100)],
            voltages: [Volts(229), Volts(231), Volts(233)],
            phases: Some(PhaseMode::Three),
            session_energy: Some(WattHours(4_200.0)),
            session_duration: Some(SessionDuration::new(Duration::from_secs(4_140))),
        }
    );
}

#[test]
fn an_idle_charger_parses() {
    let state = parse(&blocks(&IDLE)).unwrap();

    assert_eq!(state.status, ChargePointStatus::Available);
    assert_eq!(state.cable, CableState::NoCable);
    assert_eq!(state.power, ChargerDraw::ZERO);
    assert_eq!(state.session_energy, Some(WattHours::ZERO));
}

#[test]
fn a_full_car_parses_as_suspended_on_one_phase() {
    let state = parse(&blocks(&SUSPENDED)).unwrap();

    assert_eq!(state.status, ChargePointStatus::SuspendedEv);
    assert_eq!(state.cable, CableState::VehicleLocked);
    assert_eq!(state.phases, Some(PhaseMode::One));
    assert_eq!(state.session_energy, Some(WattHours(20_000.0)));
}

/// 70 000 is `0x0001_1170`: read low word first it would be 4464 × 65536.
#[test]
fn a_u32_is_read_high_word_first() {
    let mut blocks = blocks(&IDLE);
    blocks.insert(
        SESSION_ENERGY,
        vec![0x0001, 0x1170, 0, 0, 0, 0, 0x0001, 0x0000],
    );

    let state = parse(&blocks).unwrap();

    assert_eq!(state.session_energy, Some(WattHours(70_000.0)));
    assert_eq!(
        state.session_duration,
        Some(SessionDuration::new(Duration::from_secs(65_536)))
    );
}

#[test]
fn the_lifetime_meter_counts_tenths_of_a_kilowatt_hour() {
    assert_eq!(
        total_energy(&blocks(&CHARGING)).unwrap(),
        WattHours(1_234_500.0)
    );
}

#[test]
fn a_status_or_cable_this_build_cannot_name_survives_as_unknown() {
    let state = parse(&blocks(&Registers {
        status: 42,
        cable: 9,
        ..CHARGING
    }))
    .unwrap();

    assert_eq!(state.status, ChargePointStatus::Unknown(42));
    assert_eq!(state.cable, CableState::Unknown(9));
}

#[test]
fn an_unknown_phase_mode_still_parses_with_its_power() {
    let state = parse(&blocks(&Registers {
        phases: 7,
        ..CHARGING
    }))
    .unwrap();

    assert_eq!(state.power, ChargerDraw::new(11_040));
    assert_eq!(state.phases, None);
}

#[test]
fn the_display_only_blocks_missing_still_parse_with_their_fields_empty() {
    let mut blocks = blocks(&CHARGING);
    for span in BEST_EFFORT_SPANS {
        blocks.0.remove(&span.start);
    }

    let state = parse(&blocks).unwrap();

    assert_eq!(state.power, ChargerDraw::new(11_040));
    assert_eq!(state.status, ChargePointStatus::Charging);
    assert_eq!(state.phases, None);
    assert_eq!(state.session_energy, None);
    assert_eq!(state.session_duration, None);
    assert!(total_energy(&blocks).is_err());
}

#[test]
fn a_register_no_read_covered_fails_the_parse() {
    let mut blocks = blocks(&CHARGING);
    blocks.insert(CURRENTS[0], vec![0; 4]);

    let err = parse(&blocks).unwrap_err();

    assert!(err.contains("was not read"), "{err}");
}

#[test]
fn ascii_decodes_high_byte_first_and_trims_padding() {
    let words = [
        u16::from_be_bytes(*b"EV"),
        u16::from_be_bytes(*b"C0"),
        u16::from_be_bytes(*b"4 "),
        0,
        0,
    ];
    assert_eq!(decode_ascii(&words), "EVC04");
    assert_eq!(decode_ascii(&[u16::from_be_bytes(*b" V"), 0x6500]), "Ve");
    assert_eq!(decode_ascii(&[0; 4]), "");
}

#[test]
fn ascii_reads_one_character_per_register() {
    // Registers 190+10 as an EVC04 answered them on 2026-10-18.
    let brand = [0x56, 0x45, 0x53, 0x54, 0x45, 0x4c, 0, 0, 0, 0];

    assert_eq!(decode_ascii(&brand), "VESTEL");
}

#[test]
fn the_capture_maps_each_read_start_to_its_words() {
    let mut blocks = RegisterBlocks::default();
    blocks.insert(1000, vec![2, 1, 0, 0, 3]);
    blocks.insert(404, vec![1]);

    let raw = blocks.capture();

    assert_eq!(raw.kind, "vestel_modbus");
    assert_eq!(raw.body, r#"{"404":[1],"1000":[2,1,0,0,3]}"#);
}

/// A Modbus TCP server answering FC4 from a register map, until the test
/// ends: enough wire to drive the real client end to end. A read starting at
/// any of `refused` gets exception 02 (illegal data address), as firmware
/// without that block would answer.
async fn fake_vestel(map: BTreeMap<u16, u16>, refused: &[u16]) -> u16 {
    fake_vestel_closing(map, refused, usize::MAX).await
}

/// [`fake_vestel`], closing each connection after `lifetime` requests, as
/// an EVC04 closes each one ~20s after opening it.
async fn fake_vestel_closing(map: BTreeMap<u16, u16>, refused: &[u16], lifetime: usize) -> u16 {
    let refused = refused.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let map = map.clone();
            let refused = refused.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 12];
                for _ in 0..lifetime {
                    if stream.read_exact(&mut request).await.is_err() {
                        return;
                    }
                    let start = u16::from_be_bytes([request[8], request[9]]);
                    let count = u16::from_be_bytes([request[10], request[11]]);
                    let mut response = vec![request[0], request[1], 0, 0];
                    if refused.contains(&start) {
                        response.extend(3u16.to_be_bytes());
                        response.extend([request[6], 0x84, 0x02]);
                    } else {
                        let words: Vec<u8> = (start..start + count)
                            .flat_map(|r| map.get(&r).copied().unwrap_or(0).to_be_bytes())
                            .collect();
                        let length = u16::try_from(3 + words.len()).unwrap();
                        let byte_count = u8::try_from(words.len()).unwrap();
                        response.extend(length.to_be_bytes());
                        response.extend([request[6], 4, byte_count]);
                        response.extend(words);
                    }
                    if stream.write_all(&response).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

fn client(port: u16) -> VestelClient {
    VestelClient::new(
        DeviceId::new("charger"),
        "127.0.0.1".to_string(),
        port,
        255,
        Duration::from_secs(5),
    )
}

#[tokio::test]
async fn a_poll_reads_parses_and_captures_over_modbus_tcp() {
    let port = fake_vestel(device_map(&CHARGING), &[]).await;
    let client = client(port);

    for _ in 0..2 {
        let reading = client.poll().await.unwrap();

        assert_eq!(reading.state, parse(&blocks(&CHARGING)).unwrap());
        assert_eq!(reading.telemetry.total_energy, Some(WattHours(1_234_500.0)));
        assert_eq!(reading.raw, Some(blocks(&CHARGING).capture()));
    }
}

#[tokio::test]
async fn a_connection_the_charger_closed_costs_no_reading() {
    // Four identity reads and one poll's five, then the charger hangs up.
    let port = fake_vestel_closing(device_map(&CHARGING), &[], 9).await;
    let client = client(port);

    for _ in 0..3 {
        let reading = client.poll().await.unwrap();

        assert_eq!(reading.state, parse(&blocks(&CHARGING)).unwrap());
    }
}

#[tokio::test]
async fn refused_display_only_blocks_still_yield_a_reading() {
    let port = fake_vestel(
        device_map(&CHARGING),
        &BEST_EFFORT_SPANS.map(|span| span.start),
    )
    .await;
    let client = client(port);

    for _ in 0..2 {
        let reading = client.poll().await.unwrap();

        assert_eq!(reading.state.power, ChargerDraw::new(11_040));
        assert_eq!(reading.state.phases, None);
        assert_eq!(reading.state.session_energy, None);
        assert_eq!(reading.state.session_duration, None);
        assert_eq!(reading.telemetry.total_energy, None);

        let mut expected = blocks(&CHARGING);
        for span in BEST_EFFORT_SPANS {
            expected.0.remove(&span.start);
        }
        assert_eq!(reading.raw, Some(expected.capture()));
    }
}

/// Power is the register the discharge guard reads, so no reading stands in
/// for it. What did arrive is still carried for the journal.
#[tokio::test]
async fn a_refused_power_or_status_block_is_a_poll_error() {
    let port = fake_vestel(device_map(&CHARGING), &[CURRENTS[0]]).await;
    let err = client(port).poll().await.unwrap_err();

    let mut arrived = RegisterBlocks::default();
    arrived.insert(
        CHARGEPOINT_STATE,
        blocks(&CHARGING).0[&CHARGEPOINT_STATE].clone(),
    );
    assert_eq!(err.raw, Some(arrived.capture()));
    assert!(err.error.contains("1008"), "{}", err.error);

    let port = fake_vestel(device_map(&CHARGING), &[CHARGEPOINT_STATE]).await;
    let err = client(port).poll().await.unwrap_err();

    assert!(err.raw.is_none());
    assert!(err.error.contains("1000"), "{}", err.error);
}

#[tokio::test]
async fn an_unreachable_charger_is_a_poll_error_every_time() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let client = client(port);

    for _ in 0..2 {
        let err = client.poll().await.unwrap_err();
        assert!(err.raw.is_none());
        assert!(err.error.contains("connecting"), "{}", err.error);
    }
}
