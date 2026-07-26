use super::*;

fn id(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
}

fn state() -> CompatState {
    let mut state = CompatState::new(
        "29<&\"'",
        vec![
            CompatInput::new(id(1), "Camera & One", "Camera"),
            CompatInput::new(id(2), "News <GT>", "GT\"").with_text_fields(vec![
                CompatTextField::new("Headline\"", "A < B & C > D 'ok' \"yes\""),
            ]),
            CompatInput::new(id(3), "Spare", "Colour"),
        ],
        id(1),
        id(2),
    )
    .unwrap();
    state.set_overlay(2, Some(id(3))).unwrap();
    state.recording = true;
    state
}

#[test]
fn http_golden_decodes_and_resolves_all_input_forms() {
    let state = state();
    assert_eq!(
        parse_http_query("?Function=PreviewInput&Input=News+%3CGT%3E", &state),
        Ok(Translation::Protocol(CommandPayload::SelectPreview {
            input: id(2)
        }))
    );
    assert_eq!(
        parse_http_query("Function=PreviewInput&Input=1", &state),
        Ok(Translation::Protocol(CommandPayload::SelectPreview {
            input: id(1)
        }))
    );
    assert_eq!(
        parse_http_query(
            "Function=PreviewInput&Input=00000000-0000-0000-0000-000000000003",
            &state
        ),
        Ok(Translation::Protocol(CommandPayload::SelectPreview {
            input: id(3)
        }))
    );
    assert_eq!(
        parse_http_query("Function=PreviewInput&Input=-1", &state),
        Ok(Translation::Protocol(CommandPayload::SelectPreview {
            input: id(1)
        }))
    );
}

#[test]
fn tcp_golden_matches_http_translation() {
    let state = state();
    assert_eq!(
        parse_tcp_line("FUNCTION PreviewInput Input=2\r\n", &state),
        parse_http_query("Function=PreviewInput&Input=2", &state)
    );
    assert_eq!(
        parse_tcp_line(
            "FUNCTION SetText Input=2&SelectedName=Headline%22&Value=Hello+world%21\r\n",
            &state
        ),
        Ok(Translation::Compat(CompatIntent::SetText {
            input: id(2),
            field: TextFieldSelector::Name("Headline\"".to_owned()),
            value: "Hello world!".to_owned(),
        }))
    );
}

#[test]
fn every_mapped_command_has_stable_output() {
    let state = state();
    let cases = [
        (
            "Function=PreviewInput&Input=0",
            Translation::Protocol(CommandPayload::SelectPreview { input: id(2) }),
        ),
        ("Function=Cut", Translation::Protocol(CommandPayload::Cut)),
        (
            "Function=Cut&Input=3&Mix=0",
            Translation::Compat(CompatIntent::CutTo { input: id(3) }),
        ),
        (
            "Function=Fade&Duration=1000",
            Translation::Compat(CompatIntent::Fade {
                input: None,
                duration_millis: 1000,
            }),
        ),
        (
            "Function=Fade&Duration=250&Input=Camera+%26+One",
            Translation::Compat(CompatIntent::Fade {
                input: Some(id(1)),
                duration_millis: 250,
            }),
        ),
        (
            "Function=OverlayInput8&Input=3",
            Translation::Compat(CompatIntent::ToggleOverlay {
                channel: 8,
                input: id(3),
            }),
        ),
        (
            "Function=SetText&Input=2&SelectedIndex=0&Value=Live",
            Translation::Compat(CompatIntent::SetText {
                input: id(2),
                field: TextFieldSelector::Index(0),
                value: "Live".to_owned(),
            }),
        ),
        (
            "Function=StartRecording",
            Translation::Compat(CompatIntent::SetRecording { enabled: true }),
        ),
        (
            "Function=StopRecording",
            Translation::Compat(CompatIntent::SetRecording { enabled: false }),
        ),
        (
            "Function=StartStreaming&Value=2",
            Translation::Compat(CompatIntent::SetStreaming {
                enabled: true,
                stream: Some(2),
            }),
        ),
        (
            "Function=StopStreaming",
            Translation::Compat(CompatIntent::SetStreaming {
                enabled: false,
                stream: None,
            }),
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(parse_http_query(query, &state), Ok(expected), "{query}");
    }
}

#[test]
fn xml_golden_is_deterministic_and_escaped() {
    assert_eq!(
        state().xml(),
        concat!(
            "<vmix><version>29&lt;&amp;&quot;&apos;</version><inputs>",
            "<input key=\"00000000-0000-0000-0000-000000000001\" number=\"1\" type=\"Camera\" title=\"Camera &amp; One\">Camera &amp; One</input>",
            "<input key=\"00000000-0000-0000-0000-000000000002\" number=\"2\" type=\"GT&quot;\" title=\"News &lt;GT&gt;\">News &lt;GT&gt;",
            "<text index=\"0\" name=\"Headline&quot;\">A &lt; B &amp; C &gt; D &apos;ok&apos; &quot;yes&quot;</text></input>",
            "<input key=\"00000000-0000-0000-0000-000000000003\" number=\"3\" type=\"Colour\" title=\"Spare\">Spare</input>",
            "</inputs><overlays><overlay number=\"1\"></overlay><overlay number=\"2\">3</overlay>",
            "<overlay number=\"3\"></overlay><overlay number=\"4\"></overlay><overlay number=\"5\"></overlay>",
            "<overlay number=\"6\"></overlay><overlay number=\"7\"></overlay><overlay number=\"8\"></overlay>",
            "</overlays><preview>2</preview><active>1</active><recording>True</recording><streaming>False</streaming></vmix>"
        )
    );
}

#[test]
fn tally_golden_includes_active_overlays_and_preview_priority() {
    let mut state = state();
    assert_eq!(state.tally(), "121");
    assert_eq!(state.tally_response(), "TALLY OK 121\r\n");
    state.set_overlay(1, Some(id(2))).unwrap();
    assert_eq!(state.tally(), "111");
}

#[test]
fn malformed_unknown_and_unsupported_are_distinct() {
    let state = state();
    assert_eq!(
        parse_http_query("Input=1", &state),
        Err(ParseError::MissingFunction)
    );
    assert_eq!(
        parse_http_query("Function=NoSuchFunction", &state),
        Err(ParseError::UnknownFunction("NoSuchFunction".to_owned()))
    );
    assert_eq!(
        parse_http_query("Function=AudioOn&Input=1", &state),
        Ok(Translation::Unsupported(UnsupportedReport {
            function: "AudioOn".to_owned(),
            reason: "the function has no semantics in the current compatibility core",
        }))
    );
    assert!(matches!(
        parse_http_query("Function=Fade", &state),
        Ok(Translation::Unsupported(_))
    ));
    assert_eq!(
        parse_http_query("Function=Cut&Function=Fade", &state),
        Err(ParseError::DuplicateParameter("Function".to_owned()))
    );
    assert_eq!(
        parse_http_query("Function=PreviewInput&Input=%GG", &state),
        Err(ParseError::InvalidPercentEncoding)
    );
    assert_eq!(
        parse_http_query("Function=PreviewInput&Input=%FF", &state),
        Err(ParseError::InvalidUtf8)
    );
    assert_eq!(
        parse_tcp_line("TALLY\r\n", &state),
        Err(ParseError::Malformed("expected FUNCTION command"))
    );
}

#[test]
fn parameters_and_values_are_validated_strictly() {
    let state = state();
    assert!(matches!(
        parse_http_query("Function=PreviewInput&Input=99", &state),
        Err(ParseError::InputNotFound(_))
    ));
    assert!(matches!(
        parse_http_query("Function=Fade&Duration=0", &state),
        Err(ParseError::InvalidParameter {
            parameter: "Duration",
            ..
        })
    ));
    assert!(matches!(
        parse_http_query(
            "Function=SetText&Input=2&SelectedName=A&SelectedIndex=0&Value=x",
            &state
        ),
        Err(ParseError::InvalidParameter { .. })
    ));
    assert!(matches!(
        parse_http_query("Function=OverlayInput1&Input=1&Mix=1", &state),
        Ok(Translation::Unsupported(_))
    ));
    assert!(matches!(
        parse_http_query("Function=Cut&Unexpected=x", &state),
        Err(ParseError::InvalidParameter { .. })
    ));
}

#[test]
fn parser_bounds_are_enforced_at_and_above_boundaries() {
    let state = state();
    let at_value_limit = format!(
        "Function=SetText&Input=2&SelectedName=A&Value={}",
        "x".repeat(MAX_PARAMETER_VALUE_BYTES)
    );
    assert!(parse_http_query(&at_value_limit, &state).is_ok());

    let over_value_limit = format!(
        "Function=SetText&Input=2&SelectedName=A&Value={}",
        "x".repeat(MAX_PARAMETER_VALUE_BYTES + 1)
    );
    assert_eq!(
        parse_http_query(&over_value_limit, &state),
        Err(ParseError::ParameterValueTooLong)
    );

    let too_many = (0..=MAX_PARAMETERS)
        .map(|index| format!("p{index}=x"))
        .collect::<Vec<_>>()
        .join("&");
    assert_eq!(
        parse_http_query(&too_many, &state),
        Err(ParseError::TooManyParameters)
    );

    assert_eq!(
        parse_http_query(&"x".repeat(MAX_HTTP_QUERY_BYTES + 1), &state),
        Err(ParseError::QueryTooLong)
    );
    assert_eq!(
        parse_tcp_line(&"x".repeat(MAX_TCP_LINE_BYTES + 1), &state),
        Err(ParseError::LineTooLong)
    );
}

#[test]
fn state_rejects_invalid_references_and_input_bounds() {
    assert_eq!(
        CompatState::new(
            "29",
            vec![CompatInput::new(id(1), "one", "Camera")],
            id(1),
            id(2)
        ),
        Err(StateError::InputNotFound)
    );
    let inputs = (1..=(MAX_INPUTS as u128 + 1))
        .map(|value| CompatInput::new(id(value), value.to_string(), "Camera"))
        .collect();
    assert_eq!(
        CompatState::new("29", inputs, id(1), id(2)),
        Err(StateError::TooManyInputs)
    );
}
