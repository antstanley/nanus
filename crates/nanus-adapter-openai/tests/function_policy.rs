//! Explicit policy preserves caller schemas and refuses incompatible strict requests before HTTP.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use futures::StreamExt as _;
use nanus_adapter_openai::{OpenAiConfig, OpenAiLlm, Protocol, ProtocolPreference, Vendor};
use nanus_domain::{Message, ToolName, ToolSchema};
use nanus_ports::{ChatRequest, LlmEvent, LlmPort as _};
use serde_json::{Value, json};

fn request(parameters: Value) -> ChatRequest {
    let mut request = ChatRequest::new("gpt-6-astra", vec![Message::user("fictional")]);
    request.max_tokens = Some(8192);
    request.context_budget = Some(64_000);
    request.tools.push(ToolSchema {
        name: ToolName::new("read").unwrap(),
        description: "Read fictional text".into(),
        parameters,
    });
    request
}
fn config(protocol: Protocol, policy: Option<bool>) -> OpenAiConfig {
    let mut config = OpenAiConfig::new(Vendor::OpenAi, "gpt-6-astra", "fictional-key");
    config
        .set_protocol_preference(ProtocolPreference::Exact(protocol))
        .unwrap();
    config.set_function_strictness(policy);
    config
}
fn optional() -> Value {
    json!({"type":"object", "properties":{"path":{"type":"string"},
        "offset":{"type":"integer"}}, "required":["path"], "additionalProperties":false})
}
fn closed(child: Value) -> Value {
    let mut schema = json!({"type":"object", "properties":{}, "required":["value"],
        "additionalProperties":false});
    schema["properties"]
        .as_object_mut()
        .unwrap()
        .insert("value".into(), child);
    schema
}

#[test]
fn explicit_false_preserves_optional_parameters_on_both_real_encoders_and_absence_restores_stock() {
    let request = request(optional());
    for protocol in [Protocol::Responses, Protocol::ChatCompletions] {
        let mut config = config(protocol, Some(false));
        assert_eq!(config.function_strictness(), Some(false));
        for policy in [Some(false), None, Some(false)] {
            config.set_function_strictness(policy);
            let adapter = OpenAiLlm::new(config.clone()).unwrap();
            let body = adapter.encode(&request);
            let tool = if protocol == Protocol::Responses {
                &body["tools"][0]
            } else {
                &body["tools"][0]["function"]
            };
            assert_eq!(tool["parameters"], request.tools[0].parameters);
            assert_eq!(
                tool.get("strict"),
                policy.as_ref().map(|_| &Value::Bool(false))
            );
            assert_eq!(tool["parameters"]["required"], json!(["path"]));
            if protocol == Protocol::Responses {
                let estimate = adapter.estimate_request(&request).unwrap();
                assert_eq!(
                    estimate.request_bytes,
                    serde_json::to_vec(&body).unwrap().len()
                );
            }
        }
    }
}

#[test]
fn explicit_true_accepts_bounded_closed_nullable_array_and_union_schemas_without_rewriting() {
    let adapter = OpenAiLlm::new(config(Protocol::Responses, Some(true))).unwrap();
    for parameters in [
        closed(json!({"type":["string","null"]})),
        closed(json!({"type":"array", "items":{"type":"integer"}})),
        closed(json!({"anyOf":[{"type":"string"},{"type":"boolean"}]})),
        closed(closed(json!({"type":"boolean"}))),
        json!({"type":"object", "properties":{}, "required":[], "additionalProperties":false}),
    ] {
        let request = request(parameters);
        assert!(adapter.estimate_request(&request).is_ok());
        let body = adapter.encode(&request);
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tools"][0]["parameters"], request.tools[0].parameters);
    }
}

#[tokio::test]
async fn incompatible_strict_schemas_are_terminal_preflight_errors_not_provider_requests() {
    let adapter = OpenAiLlm::new(config(Protocol::Responses, Some(true))).unwrap();
    for parameters in [
        optional(),
        closed(json!({"type":"elephant"})),
        closed(json!({"type":[]})),
        closed(json!({"type":["string","string"]})),
        closed(json!({"type":"array"})),
        closed(json!({"type":"string", "items":{}})),
        closed(json!({"type":"string", "enum":[]})),
        closed(json!({"type":"string", "description":4})),
        closed(json!({"$ref":"#/$defs/unchecked"})),
        closed(json!({"anyOf":[]})),
        closed(
            json!({"type":"object", "properties":{}, "required":[], "additionalProperties":true}),
        ),
        json!({"type":"object", "properties":{"a":{"type":"string"},"b":{"type":"string"}},
            "required":["a","a"], "additionalProperties":false}),
    ] {
        let request = request(parameters);
        assert!(adapter.estimate_request(&request).is_err());
        let events: Vec<_> = adapter.stream_chat(request).collect().await;
        assert!(matches!(events.as_slice(), [LlmEvent::Error(_)]));
    }
}

#[tokio::test]
async fn unknown_vendor_explicit_policy_refuses_even_without_a_context_budget() {
    let mut config = OpenAiConfig::new(Vendor::Zai, "glm-5.3", "fictional-key");
    config.set_function_strictness(Some(false));
    let adapter = OpenAiLlm::new(config).unwrap();
    let mut request = request(optional());
    request.model = "glm-5.3".into();
    request.context_budget = None;
    assert!(adapter.estimate_request(&request).is_err());
    let events: Vec<_> = adapter.stream_chat(request).collect().await;
    assert!(matches!(events.as_slice(), [LlmEvent::Error(_)]));
}

#[test]
fn iterative_strict_validation_accepts_exact_node_and_depth_bounds_then_refuses_next_units() {
    let adapter = OpenAiLlm::new(config(Protocol::Responses, Some(true))).unwrap();
    for (count, accepted) in [(4999, true), (5000, true), (5001, false)] {
        let properties: serde_json::Map<String, Value> = (0..count)
            .map(|i| (format!("p{i}"), json!({"type":"boolean"})))
            .collect();
        let required: Vec<_> = properties.keys().cloned().collect();
        let parameters = json!({"type":"object", "properties":properties, "required":required,
            "additionalProperties":false});
        let mut request = request(parameters);
        request.context_budget = None;
        assert_eq!(adapter.estimate_request(&request).is_ok(), accepted);
    }
    for (depth, accepted) in [(7, true), (8, true), (9, false)] {
        let mut child = json!({"type":"boolean"});
        for _ in 0..depth {
            child = json!({"type":"array", "items":child});
        }
        assert_eq!(
            adapter.estimate_request(&request(closed(child))).is_ok(),
            accepted
        );
    }
    for (count, accepted) in [(16_382, true), (16_383, false)] {
        let alternatives = vec![json!({"type":"boolean"}); count];
        let mut request = request(closed(json!({"anyOf":alternatives})));
        request.context_budget = None;
        assert_eq!(adapter.estimate_request(&request).is_ok(), accepted);
    }
}

#[test]
fn strict_enum_and_aggregate_unicode_string_limits_accept_the_bound_then_refuse_next_unit() {
    let adapter = OpenAiLlm::new(config(Protocol::Responses, Some(true))).unwrap();
    for (count, accepted) in [(999, true), (1000, true), (1001, false)] {
        let values: Vec<_> = (0..count).map(|i| format!("v{i}")).collect();
        assert_eq!(
            adapter
                .estimate_request(&request(closed(json!({"type":"string", "enum":values}))))
                .is_ok(),
            accepted
        );
    }
    for (chars, accepted) in [(14_999_usize, true), (15_000, true), (15_001, false)] {
        let mut values: Vec<String> = (0..251).map(|i| format!("v{i}")).collect();
        let existing: usize = values.iter().map(String::len).sum();
        values[0].push_str(&"x".repeat(chars.checked_sub(existing).unwrap()));
        assert_eq!(
            adapter
                .estimate_request(&request(closed(json!({"type":"string", "enum":values}))))
                .is_ok(),
            accepted
        );
    }
    for (chars, accepted) in [(119_999, true), (120_000, true), (120_001, false)] {
        let name = "é".repeat(chars);
        let schema = json!({"type":"object","properties":{name.clone():{"type":"boolean"}},
            "required":[name],"additionalProperties":false});
        assert_eq!(adapter.estimate_request(&request(schema)).is_ok(), accepted);
    }
    for child in [
        json!({"type":"string","enum":["same","same"]}),
        json!({"type":"string","enum":["first",1]}),
        json!({"type":"integer","enum":[1.5]}),
        json!({"type":"boolean","enum":[null]}),
    ] {
        assert!(adapter.estimate_request(&request(closed(child))).is_err());
    }
}
