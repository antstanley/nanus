use super::*;

fn limits(line: usize, event: usize, total: usize, count: usize) -> ResponseLimits {
    ResponseLimits::new(line, event, total, count, 2, 8).expect("consistent limits")
}
fn reader() -> ResponseFrames {
    ResponseFrames::new(Some(limits(32, 16, 256, 4)))
}

#[test]
fn budgets_refuse_zero_inconsistency_and_excessive_slots() {
    for (line, event, total, count, slots, error) in [
        (0, 1, 8, 1, 1, 1),
        (1, 0, 8, 1, 1, 1),
        (1, 1, 0, 1, 1, 1),
        (1, 1, 8, 0, 1, 1),
        (1, 1, 8, 1, 0, 1),
        (1, 1, 8, 1, 1, 0),
        (9, 1, 8, 1, 1, 1),
        (1, 9, 8, 1, 1, 1),
        (1, 1, 8, 1, 257, 1),
        (1, 1, 8, 1, 1, 9),
    ] {
        assert!(ResponseLimits::new(line, event, total, count, slots, error).is_err());
    }
    assert!(ResponseLimits::new(8, 8, 8, 1, 256, 8).is_ok());
    assert!(ResponseLimits::add("bytes", usize::MAX, 1, usize::MAX).is_err());
}
#[test]
fn calls_check_combined_content_and_indexes_without_folding() {
    let limits = limits(32, 16, 256, 4);
    assert!(limits.index(1).is_ok());
    assert!(limits.index(2).is_err());
    assert!(limits.index(u64::MAX).is_err());
    assert!(limits.call_bytes("id", "name", 10).is_ok());
    assert!(limits.call_bytes("id", "name", 11).is_err());
}
#[test]
fn fragmented_utf8_is_decoded_only_after_the_complete_line() {
    let mut frames = reader();
    assert!(
        frames
            .push(b"data: \xc3")
            .expect("partial UTF-8")
            .is_empty()
    );
    assert_eq!(frames.push(b"\xa9\r\n").expect("complete"), ["é"]);
    assert!(frames.finish().expect("empty tail").is_none());
}
#[test]
fn invalid_utf8_is_refused_and_failure_cannot_be_reused() {
    let mut frames = reader();
    assert!(frames.push(b"data: \xff\n").is_err());
    assert!(frames.push(b"data: {}\n").is_err());
    assert!(frames.finish().is_err());
}
#[test]
fn line_limit_accepts_the_boundary_and_refuses_the_next_fragment() {
    let mut frames = ResponseFrames::new(Some(limits(8, 8, 64, 8)));
    assert_eq!(frames.push(b"data: {}\n").expect("exact line"), ["{}"]);
    assert!(frames.push(b"data: {}").expect("exact partial").is_empty());
    assert!(frames.push(b"x").is_err());
}
#[test]
fn many_short_lines_do_not_share_the_line_budget() {
    let mut frames = ResponseFrames::new(Some(limits(8, 8, 64, 8)));
    assert_eq!(
        frames
            .push(b"data: {}\ndata: {}\ndata: {}\n")
            .expect("short lines"),
        ["{}", "{}", "{}"]
    );
}

#[test]
fn raw_response_counts_comments_and_rejects_a_whole_chunk_before_retaining_it() {
    let mut frames = ResponseFrames::new(Some(limits(8, 8, 8, 8)));
    assert!(frames.push(b":123456\n").expect("exact total").is_empty());
    assert!(frames.push(b"\n").is_err());
    assert!(reader().push(&[b'x'; 257]).is_err());
}
#[test]
fn data_payload_size_is_checked_before_it_is_owned() {
    let mut frames = ResponseFrames::new(Some(limits(32, 2, 64, 8)));
    assert_eq!(frames.push(b"data: {}\n").expect("exact payload"), ["{}"]);
    assert!(frames.push(b"data: abc\n").is_err());
}
#[test]
fn unknown_payloads_count_and_a_failing_batch_is_not_returned_partially() {
    let mut frames = ResponseFrames::new(Some(limits(32, 16, 256, 2)));
    assert_eq!(
        frames.push(b"data: {}\ndata: {}\n").expect("exact count"),
        ["{}", "{}"]
    );
    assert!(frames.push(b"data: {}\n").is_err());
    let mut frames = ResponseFrames::new(Some(limits(32, 16, 256, 1)));
    assert!(frames.push(b"data: {}\ndata: {}\n").is_err());
}
#[test]
fn sentinels_and_tails_are_consumed_once() {
    let mut frames = reader();
    assert_eq!(
        frames
            .push(b"data: {}\ndata: [DONE]\ndata: garbage\n")
            .expect("sentinel"),
        ["{}"]
    );
    assert!(frames.is_done());
    assert!(frames.finish().expect("done").is_none());
    let mut frames = reader();
    assert!(frames.push(b"data: [DONE]").expect("tail").is_empty());
    assert!(frames.finish().expect("sentinel tail").is_none());
    assert!(frames.is_done());
    let mut frames = reader();
    frames.push(b"data: {}").expect("tail");
    assert_eq!(frames.finish().expect("tail payload"), Some("{}".into()));
    assert!(frames.finish().expect("once").is_none());
}
#[test]
fn absent_policy_retains_legacy_lossy_framing() {
    let mut frames = ResponseFrames::new(None);
    assert_eq!(frames.push(b"data: \xff\n").expect("legacy"), ["�"]);
    assert_eq!(
        frames.push(&[b'x'; 257]).expect("no budget"),
        Vec::<String>::new()
    );
}
