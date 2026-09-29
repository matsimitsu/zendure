use super::*;

fn dataset_json(soc_value: &str) -> String {
    format!(
        r#"{{
            "vin": "WVWZZZAAZ00000000",
            "user_id": "abc",
            "Data": [
                {{"key": "11111111-1111-1111-1111-111111111111", "dataFieldName": "mileage.value", "value": "12345", "timestampUtc": "2026-09-29T10:00:00Z"}},
                {{"key": "22222222-2222-2222-2222-222222222222", "dataFieldName": "battery_state_report.soc", "value": "{soc_value}", "timestampUtc": "2026-09-29T10:00:00Z"}}
            ]
        }}"#
    )
}

fn zip_bytes(entry_name: &str, json: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut writer = zip::ZipWriter::new(cursor);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file(entry_name, options).unwrap();
        std::io::Write::write_all(&mut writer, json.as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    buf
}

#[test]
fn extract_soc_from_json_reads_the_dotted_battery_soc_field() {
    let json = dataset_json("62");
    assert_eq!(extract_soc_from_json(&json).unwrap(), Soc::new(62));
}

#[test]
fn extract_soc_from_json_tolerates_a_trailing_percent_sign() {
    let json = dataset_json("62%");
    assert_eq!(extract_soc_from_json(&json).unwrap(), Soc::new(62));
}

#[test]
fn extract_soc_from_json_fails_without_a_data_array() {
    let result = extract_soc_from_json(r#"{"vin": "x"}"#);
    assert!(result.is_err());
}

#[test]
fn extract_soc_from_json_fails_when_the_soc_field_is_absent() {
    let json = r#"{"Data": [{"key": "1", "dataFieldName": "mileage.value", "value": "1"}]}"#;
    assert!(extract_soc_from_json(json).is_err());
}

#[test]
fn extract_soc_from_json_fails_on_an_unparseable_value() {
    let json = dataset_json("not-a-number");
    assert!(extract_soc_from_json(&json).is_err());
}

#[test]
fn extract_soc_unzips_and_reads_the_one_json_member() {
    let zip = zip_bytes("dataset.json", &dataset_json("47"));
    assert_eq!(extract_soc(&zip).unwrap(), Soc::new(47));
}

#[test]
fn extract_soc_fails_on_a_zip_with_no_json_member() {
    let mut buf = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut writer = zip::ZipWriter::new(cursor);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("readme.txt", options).unwrap();
        std::io::Write::write_all(&mut writer, b"nothing here").unwrap();
        writer.finish().unwrap();
    }
    assert!(extract_soc(&buf).is_err());
}

#[test]
fn extract_soc_fails_on_bytes_that_are_not_a_zip() {
    assert!(extract_soc(b"not a zip file").is_err());
}

#[test]
fn extract_template_model_parses_the_idk_js_blob() {
    let html = r#"
        <html><body>
        <script>
        window._IDK = {
            templateModel: {"hmac": "abc123", "relayState": "rs-1", "emailPasswordForm": {"email": "me@example.com"}},
            csrf_token: 'tok-1'
        };
        </script>
        </body></html>
    "#;
    let model = extract_template_model(html);
    assert_eq!(model["hmac"], "abc123");
    assert_eq!(model["relayState"], "rs-1");
    assert_eq!(model["emailPasswordForm"]["email"], "me@example.com");
}

#[test]
fn extract_template_model_is_null_when_absent() {
    assert_eq!(extract_template_model("<html></html>"), Value::Null);
}

#[test]
fn extract_csrf_reads_the_token_out_of_the_js() {
    let html = "window._IDK = { csrf_token: 'tok-42' };";
    assert_eq!(extract_csrf(html), Some("tok-42".to_string()));
}

#[test]
fn extract_csrf_is_none_when_absent() {
    assert_eq!(extract_csrf("<html></html>"), None);
}

#[test]
fn parse_form_reads_the_action_and_hidden_inputs() {
    let html = r#"
        <html><body>
        <form action="/signin-service/v1/authenticate" method="post">
            <input type="hidden" name="hmac" value="abc123">
            <input type="hidden" name="_csrf" value="csrf-1" />
            <input type="email" name="email" value="">
        </form>
        </body></html>
    "#;
    let (action, fields) = parse_form(html);
    assert_eq!(action.as_deref(), Some("/signin-service/v1/authenticate"));
    assert_eq!(fields.get("hmac").map(String::as_str), Some("abc123"));
    assert_eq!(fields.get("_csrf").map(String::as_str), Some("csrf-1"));
    assert_eq!(fields.get("email").map(String::as_str), Some(""));
}

#[test]
fn parse_form_is_empty_when_no_form_is_present() {
    let (action, fields) = parse_form("<html><body>no form here</body></html>");
    assert_eq!(action, None);
    assert!(fields.is_empty());
}

#[test]
fn login_fields_merges_html_inputs_with_the_template_model() {
    let html = r#"
        <html><body>
        <form action="/next-step">
            <input type="hidden" name="_csrf" value="csrf-from-html">
        </form>
        <script>
        window._IDK = { templateModel: {"hmac": "hmac-from-js", "relayState": "rs-1"} };
        </script>
        </body></html>
    "#;
    let (fields, action) = login_fields(html);
    assert_eq!(action.as_deref(), Some("/next-step"));
    assert_eq!(fields.get("hmac").map(String::as_str), Some("hmac-from-js"));
    assert_eq!(fields.get("relayState").map(String::as_str), Some("rs-1"));
    assert_eq!(
        fields.get("_csrf").map(String::as_str),
        Some("csrf-from-html")
    );
}

#[test]
fn resolve_joins_a_relative_path_against_the_base() {
    let resolved = resolve(
        "https://identity.vwgroup.io/signin-service/v1/signin",
        "/authenticate?relayState=x",
    )
    .unwrap();
    assert_eq!(
        resolved,
        "https://identity.vwgroup.io/authenticate?relayState=x"
    );
}

#[test]
fn resolve_strips_the_query_when_the_action_is_empty() {
    let resolved = resolve("https://identity.vwgroup.io/authenticate?relayState=x", "").unwrap();
    assert_eq!(resolved, "https://identity.vwgroup.io/authenticate");
}

#[test]
fn accept_language_builds_a_region_tagged_header() {
    assert_eq!(accept_language("de", "de"), "de-DE,de;q=0.9,en;q=0.8");
}

#[test]
fn passed_portal_callback_finds_the_callback_hop_anywhere_in_history() {
    let history = vec![
        "https://identity.vwgroup.io/oidc/v1/authorize?x=1".to_string(),
        format!("{BASE_URL}/services/callbacklogin?code=abc"),
        format!("{BASE_URL}/de/some-landing-page"),
    ];
    assert!(passed_portal_callback(&history));
}

#[test]
fn passed_portal_callback_is_false_without_the_callback_hop() {
    let history = vec!["https://identity.vwgroup.io/signin-service/v1/signin".to_string()];
    assert!(!passed_portal_callback(&history));
}
