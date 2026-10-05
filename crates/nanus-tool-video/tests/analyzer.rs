//! The analysis route against a scripted model: what it sends, and what it refuses to keep.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]
// Test arithmetic is over small literals and counters, `FFmpeg` is a name, and the two
// `let shared` bindings are what make an `Rc` unsize to a trait object.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::doc_markdown,
    clippy::let_and_return
)]

use base64::Engine as _;
use std::cell::RefCell;
use std::rc::Rc;

use nanus_domain::{ContentBlock, Message, Usage};
use nanus_ports::{
    ChatRequest, FinishReason, ImageInputSupport, ImageProfile, LlmEvent, LlmPort, LlmStream,
    ModelCapabilities,
};
use nanus_tool_video::{
    AnalysisRequest, LlmAnalyzer, Provenance, SampledFrame, VideoAnalyzer, VideoError, Window,
};

struct Scripted {
    events: Vec<LlmEvent>,
    requests: Rc<RefCell<Vec<ChatRequest>>>,
    supported: bool,
}

impl LlmPort for Scripted {
    fn model(&self) -> &'static str {
        "gpt-6-luna"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if !self.supported {
            return ModelCapabilities::default();
        }
        ModelCapabilities {
            image_input: ImageInputSupport::Supported,
            image_profile: ImageProfile::for_openai_model(model),
            context_window_tokens: Some(200_000),
            max_input_tokens: Some(200_000),
            max_output_tokens: Some(128_000),
        }
    }
    fn stream_chat(&self, request: ChatRequest) -> LlmStream {
        self.requests.borrow_mut().push(request);
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

/// A model whose response never arrives, so a request is genuinely in flight.
struct Hanging;

impl LlmPort for Hanging {
    fn model(&self) -> &'static str {
        "gpt-6-luna"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        Scripted {
            events: Vec::new(),
            requests: Rc::default(),
            supported: true,
        }
        .capabilities(model)
    }
    fn stream_chat(&self, _: ChatRequest) -> LlmStream {
        Box::pin(futures::stream::pending())
    }
}

fn provenance() -> Provenance {
    Provenance {
        provider: "openai".into(),
        plan: "api".into(),
        model: String::new(),
        endpoint_origin: "https://api.openai.com".into(),
        protocol: "responses".into(),
        profile_version: String::new(),
        processing: "one request".into(),
    }
}

fn jpeg(shade: u8) -> Vec<u8> {
    let pixels = image::RgbImage::from_pixel(8, 8, image::Rgb([shade, shade, shade]));
    let mut out = std::io::Cursor::new(Vec::new());
    pixels
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .expect("encode");
    out.into_inner()
}

fn request() -> AnalysisRequest {
    AnalysisRequest {
        question: "what changed?".into(),
        window: Window {
            start_ms: 1000,
            end_ms: 9000,
        },
        frames: [(2000, 10), (4000, 120), (6000, 240)]
            .into_iter()
            .map(|(timestamp_ms, shade)| SampledFrame {
                timestamp_ms,
                width: 8,
                height: 8,
                jpeg: jpeg(shade),
            })
            .collect(),
    }
}

fn analyze(events: Vec<LlmEvent>) -> (Result<String, VideoError>, Vec<ChatRequest>) {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Scripted {
        events,
        requests: Rc::clone(&requests),
        supported: true,
    }));
    let analyzer = LlmAnalyzer::new(llm, "gpt-6-luna", provenance()).expect("a verified model");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime
        .block_on(analyzer.analyze(&request()))
        .map(|analysis| analysis.answer);
    let sent = requests.borrow().clone();
    (result, sent)
}

fn answer(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(text.into()),
        LlmEvent::Usage(Usage::new(900, 40, 12, 0, 900)),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ]
}

#[test]
fn exactly_the_sampled_jpegs_and_their_labels_are_sent_once() {
    let (result, sent) = analyze(answer("a bar fills"));
    assert_eq!(result.expect("an answer"), "a bar fills");
    assert_eq!(sent.len(), 1, "one request");
    let chat = &sent[0];
    assert_eq!(chat.model, "gpt-6-luna");
    assert_eq!(chat.max_tokens, Some(2048));
    assert!(chat.tools.is_empty());
    assert_eq!(chat.messages.len(), 2);
    let Some(Message::User {
        content_blocks: Some(blocks),
        ..
    }) = chat.messages.last()
    else {
        panic!("the final message must be typed human input");
    };
    assert_eq!(
        blocks.first(),
        Some(&ContentBlock::Text("what changed?".into()))
    );
    let sent_images: Vec<Vec<u8>> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data_base64, .. } => Some(
                base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .expect("base64"),
            ),
            ContentBlock::Text(_) => None,
        })
        .collect();
    let expected: Vec<Vec<u8>> = request()
        .frames
        .into_iter()
        .map(|frame| frame.jpeg)
        .collect();
    assert_eq!(sent_images, expected, "byte-identical, in time order");
    let labels: Vec<_> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) if text.starts_with("frame ") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        [
            "frame 1 at 2000 ms",
            "frame 2 at 4000 ms",
            "frame 3 at 6000 ms"
        ]
    );
}

#[test]
fn a_cut_off_empty_or_misbehaving_answer_is_a_failure_not_a_result() {
    let finished = |reason| LlmEvent::Finished { reason };
    let cases: [(&str, Vec<LlmEvent>, &str); 6] = [
        (
            "cut off at the ceiling",
            vec![
                LlmEvent::TextDelta("half".into()),
                finished(FinishReason::Length),
            ],
            "cut off",
        ),
        ("no text", vec![finished(FinishReason::Stop)], "no text"),
        (
            "a tool call",
            vec![
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments_delta: "{}".into(),
                },
                finished(FinishReason::ToolCalls),
            ],
            "call a tool",
        ),
        (
            "a provider error",
            vec![LlmEvent::Error("HTTP 429".into())],
            "HTTP 429",
        ),
        (
            "an answer over the byte bound",
            vec![
                LlmEvent::TextDelta("x".repeat(30_000)),
                finished(FinishReason::Stop),
            ],
            "answer bound",
        ),
        (
            "a stream that just stops",
            vec![LlmEvent::TextDelta("partial".into())],
            "without finishing",
        ),
    ];
    for (name, events, reason) in cases {
        let (result, _) = analyze(events);
        let error = result.expect_err(name).to_string();
        assert!(error.contains(reason), "{name}: {error}");
    }
}

#[test]
fn a_model_without_verified_image_input_is_never_an_analysis_route() {
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Scripted {
        events: Vec::new(),
        requests: Rc::default(),
        supported: false,
    }));
    let Err(error) = LlmAnalyzer::new(llm, "gpt-6-luna", provenance()) else {
        panic!("an unverified model must not become a route");
    };
    assert!(matches!(error, VideoError::Unavailable(_)), "{error}");
    assert!(error.to_string().contains("gpt-6-luna"), "{error}");
}

/// Runs one analysis under `budget`, returning whether it succeeded.
fn analyze_under(budget: &Rc<nanus_tool_video::AnalysisBudget>, events: Vec<LlmEvent>) -> bool {
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Scripted {
        events,
        requests: Rc::default(),
        supported: true,
    }));
    let analyzer = LlmAnalyzer::new(llm, "gpt-6-luna", provenance())
        .expect("a verified model")
        .with_budget(Rc::clone(budget))
        .expect("the budget covers one answer");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(analyzer.analyze(&request())).is_ok()
}

#[test]
fn a_reported_usage_settles_the_charge_and_anything_else_keeps_the_whole_reservation() {
    let budget = nanus_tool_video::AnalysisBudget::new(1_000_000);
    assert!(analyze_under(&budget, answer("ok")));
    // Usage::new(900, 40, ..) is 940 tokens: the reservation is refunded down to it.
    assert_eq!(budget.spent(), 940);
    assert_eq!(budget.remaining(), 1_000_000 - 940);

    // No usage report: the full reservation (estimated input plus the 2048 ceiling) stays charged.
    let before = budget.spent();
    let no_usage = vec![
        LlmEvent::TextDelta("ok".into()),
        LlmEvent::Finished {
            reason: FinishReason::Stop,
        },
    ];
    assert!(analyze_under(&budget, no_usage));
    assert!(
        budget.spent() - before >= 2048,
        "{}",
        budget.spent() - before
    );

    // A failed request is charged in full too: local failure does not stop remote generation.
    let before = budget.spent();
    assert!(!analyze_under(
        &budget,
        vec![LlmEvent::Error("boom".into())]
    ));
    assert!(budget.spent() - before >= 2048);
    assert_eq!(budget.spent() + budget.remaining(), 1_000_000);
}

#[test]
fn a_dropped_request_keeps_its_reservation() {
    use futures::FutureExt as _;
    let budget = nanus_tool_video::AnalysisBudget::new(1_000_000);
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Hanging));
    let analyzer = LlmAnalyzer::new(llm, "gpt-6-luna", provenance())
        .expect("a verified model")
        .with_budget(Rc::clone(&budget))
        .expect("covers one answer");
    let frames = request();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut future = Box::pin(analyzer.analyze(&frames));
        // Polling once reserves and starts the stream; dropping then models a cancelled call.
        assert!(
            (&mut future).now_or_never().is_none(),
            "the request is still in flight"
        );
        drop(future);
    });
    assert!(
        budget.spent() >= 2048,
        "the cancelled request is still charged"
    );
}

#[test]
fn a_budget_that_cannot_cover_an_answer_refuses_before_any_request() {
    let tiny = nanus_tool_video::AnalysisBudget::new(100);
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Scripted {
        events: answer("never sent"),
        requests: Rc::default(),
        supported: true,
    }));
    let Err(error) = LlmAnalyzer::new(llm, "gpt-6-luna", provenance())
        .expect("a verified model")
        .with_budget(tiny)
    else {
        panic!("a 100-token budget cannot cover a 2048-token answer");
    };
    assert!(error.to_string().contains("analysis budget"), "{error}");
}

/// Admission is the worst case known before the video is opened: four full-size stills can be
/// refused by a budget that covers one answer, and one still on the same budget is admitted.
#[test]
fn admission_refuses_what_the_budget_cannot_cover_at_full_size() {
    let llm: nanus_ports::LlmHandle = Rc::new(Box::new(Scripted {
        events: Vec::new(),
        requests: Rc::default(),
        supported: true,
    }));
    let budget = nanus_tool_video::AnalysisBudget::new(6_000);
    let analyzer = LlmAnalyzer::new(llm, "gpt-6-luna", provenance())
        .expect("a verified model")
        .with_budget(Rc::clone(&budget))
        .expect("the budget covers one answer");
    let error = analyzer
        .admit("what changed?", 4)
        .expect_err("four 1024-pixel stills and an answer exceed 6000 tokens");
    assert!(error.to_string().contains("analysis budget"), "{error}");
    analyzer
        .admit("what changed?", 1)
        .expect("one still and an answer fit in 6000 tokens");
    assert_eq!(budget.spent(), 0, "admission reserves nothing");

    // With no budget there is nothing to refuse against.
    let hanging: nanus_ports::LlmHandle = Rc::new(Box::new(Hanging));
    let unbudgeted =
        LlmAnalyzer::new(hanging, "gpt-6-luna", provenance()).expect("a verified model");
    assert!(unbudgeted.admit("what changed?", 4).is_ok());
}
