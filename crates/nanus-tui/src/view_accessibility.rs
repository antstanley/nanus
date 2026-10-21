//! Regression tests for small screens, visible focus and whole-frame monochrome styling.

use super::*;
use ratatui::Terminal;
use ratatui::backend::{Backend as _, TestBackend};
use ratatui::style::Color;
use unicode_width::UnicodeWidthChar as _;

fn draw(state: &mut ViewState, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal.draw(|frame| state.render(frame)).expect("draw");
    terminal.backend().buffer().clone()
}

fn text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .map(|row| {
            (0..buffer.area.width)
                .filter_map(|column| buffer.cell((column, row)))
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn approval() -> ViewState {
    let mut state = ViewState::new();
    state.pending_approval = Some(PendingApproval {
        call_id: "audit".into(),
        tool: "bash".into(),
        reason: Some(format!(
            "{} FINAL_REASON",
            "a sandbox exception needs careful review ".repeat(20)
        )),
    });
    state
}

#[test]
fn long_approval_reasons_cannot_displace_decisions() {
    let mut state = approval();
    let first = text(&draw(&mut state, 40, 12));
    for label in [
        "Selected: deny",
        "y allow once",
        "a always allow",
        "n/Esc deny",
    ] {
        assert!(
            first.contains(label),
            "{label} must remain visible: {first}"
        );
    }
    assert!(!first.contains("FINAL_REASON"));
    state.dialog_scroll = u16::MAX;
    let last = text(&draw(&mut state, 40, 12));
    assert!(
        last.contains("FINAL_REASON"),
        "the reason can be read to its end: {last}"
    );
    assert!(
        last.contains("n/Esc deny"),
        "actions never scroll away: {last}"
    );
}

#[test]
fn every_chooser_reveals_its_last_selected_entry_on_short_screens() {
    for modal in [Modal::Models, Modal::Effort, Modal::Provider, Modal::Queue] {
        let mut state = ViewState::new();
        for index in 0..20 {
            let name = format!("choice-{index:02}");
            state.models.push(name.clone());
            state.effort_levels.push(name.clone());
            state.queue.push(name.clone());
            state.providers.push(ProviderChoice {
                provider: name.clone(),
                plan: "api".into(),
                label: name,
                env: "KEY".into(),
                refused: None,
            });
        }
        state.model_selection = 19;
        state.effort_selection = 19;
        state.provider_selection = 19;
        state.queue_selection = 19;
        match modal {
            Modal::Models => state.model_open = true,
            Modal::Effort => state.effort_open = true,
            Modal::Provider => state.provider_open = true,
            _ => state.queue_open = true,
        }
        for (width, height) in [(80, 12), (24, 10), (40, 8)] {
            let drawn = text(&draw(&mut state, width, height));
            assert!(
                drawn.contains("▶ 20 choice-19"),
                "{modal:?} at {width}x{height}: {drawn}"
            );
            assert!(drawn.contains("Esc"), "the exit is visible too: {drawn}");
        }
    }
}

#[test]
fn long_names_and_help_descriptions_are_reachable_without_horizontal_clipping() {
    let mut state = ViewState::new();
    state.models = vec![format!("{} FINAL_NAME", "long_model_".repeat(30))];
    state.open_models();
    let first = text(&draw(&mut state, 24, 12));
    assert!(
        first.contains('▶'),
        "the selection's start is revealed: {first}"
    );
    state.dialog_scroll = u16::MAX;
    let last = text(&draw(&mut state, 24, 12));
    assert!(
        last.contains("FINAL_NAME"),
        "the full name is reachable: {last}"
    );
    state.close_models();
    state.open_help();
    let mut pages = String::new();
    for _ in 0..200 {
        pages.push_str(&text(&draw(&mut state, 24, 12)));
        state.scroll_help(1);
    }
    let compact: String = pages.chars().filter(|c| c.is_alphanumeric()).collect();
    assert!(compact.contains("cooperation"));
    assert!(compact.contains("transcript"));
    assert!(compact.contains("denyit"));
    assert!(!pages.contains('…'), "the help does not discard words");
}

/// Every screen the interface can be showing: the conversation alone, and under each dialog.
const SCREENS: [Option<Modal>; 12] = [
    None,
    Some(Modal::Approval),
    Some(Modal::Permissions),
    Some(Modal::Context),
    Some(Modal::Models),
    Some(Modal::Effort),
    Some(Modal::Provider),
    Some(Modal::Queue),
    Some(Modal::Help),
    Some(Modal::Credential),
    Some(Modal::KeyEntry),
    Some(Modal::Authorization),
];

/// A view with something of every kind in it: markdown, a list for each chooser, a queued prompt.
fn busy_screen() -> ViewState {
    let mut state = ViewState::new();
    state.transcript.push(Entry::prose(
        Role::Assistant,
        "# Heading\n\n[link](https://example.org)\n\n```rust\n// comment\nlet n = 3;\n```",
    ));
    state.models = vec!["model".into()];
    state.effort_levels = vec!["high".into()];
    state.providers = vec![ProviderChoice {
        provider: "vendor".into(),
        plan: "api".into(),
        label: "vendor".into(),
        env: "KEY".into(),
        refused: None,
    }];
    state.queue.push("queued draft".into());
    state
}

/// Opens exactly one dialog, or none.
fn show(state: &mut ViewState, modal: Option<Modal>) {
    state.pending_approval = if modal == Some(Modal::Approval) {
        approval().pending_approval
    } else {
        None
    };
    state.permission_open = modal == Some(Modal::Permissions);
    state.context_dialog = (modal == Some(Modal::Context))
        .then(|| crate::context::ContextDialog::new(state.context_mode, None));
    state.model_open = modal == Some(Modal::Models);
    state.effort_open = modal == Some(Modal::Effort);
    state.provider_open = modal == Some(Modal::Provider);
    state.queue_open = modal == Some(Modal::Queue);
    state.help_open = modal == Some(Modal::Help);
    state.credential_open = modal == Some(Modal::Credential);
    state.key_entry_open = modal == Some(Modal::KeyEntry);
    state.auth_open = modal == Some(Modal::Authorization);
}

#[test]
fn monochrome_covers_every_dialog_and_markdown_not_just_transcript_roles() {
    let mut state = busy_screen();
    for modal in SCREENS {
        show(&mut state, modal);
        for theme in [
            Theme::monochrome(),
            Theme::light().without_color(),
            Theme::dark().without_color(),
        ] {
            state.theme = theme;
            let buffer = draw(&mut state, 80, 24);
            for cell in &buffer.content {
                assert_eq!(cell.fg, Color::Reset, "foreground in {modal:?}");
                assert_eq!(cell.bg, Color::Reset, "background in {modal:?}");
                assert!(
                    !cell.modifier.contains(Modifier::DIM),
                    "important text is never dimmed"
                );
            }
        }
    }
}

/// The other direction. An explicit palette owns its background, so its contrast is a property of
/// the pair — which is only true if no cell is left to the terminal: one with the terminal's own
/// background under the palette's text is exactly the unreadable pair the palette exists to rule
/// out. Every cell that reaches the terminal carries both of the palette's colours.
#[test]
fn explicit_palettes_colour_every_cell_of_every_screen() {
    for theme in [Theme::light(), Theme::dark()] {
        let background = theme.assistant.bg.expect("a palette has a background");
        let mut state = busy_screen();
        state.theme = theme;
        // Wide and joined glyphs too, in the transcript and under the caret.
        state
            .transcript
            .push(Entry::prose(Role::User, "wide 界界 and joined 👩‍💻 text"));
        state.input.insert_str("界 👩‍💻");
        for modal in SCREENS {
            show(&mut state, modal);
            for (width, height) in [(80, 24), (40, 12)] {
                let buffer = draw(&mut state, width, height);
                for row in 0..height {
                    // The cell after a wide glyph is never sent: the terminal draws the glyph
                    // across both, in the glyph's colours, so the backend's copy of it stays
                    // blank and is not a cell of its own to check.
                    let mut covered = 0_usize;
                    for column in 0..width {
                        let cell = buffer.cell((column, row)).expect("a cell");
                        if covered > 0 {
                            covered = covered.saturating_sub(1);
                            continue;
                        }
                        covered = cell.symbol().width().saturating_sub(1);
                        let at = format!("{modal:?} {width}x{height} at {column},{row}");
                        assert_eq!(cell.bg, background, "background in {at}");
                        assert_ne!(cell.fg, Color::Reset, "foreground in {at}");
                    }
                }
            }
        }
    }
}

#[test]
fn masked_key_entry_reveals_the_field_after_typing_on_a_short_screen() {
    let mut state = ViewState::new();
    state.open_credential("vendor", Some("api"), "KEY");
    state.open_key_entry();
    let first = text(&draw(&mut state, 24, 10));
    assert!(
        first.contains('>'),
        "the field is initially visible: {first}"
    );
    state.dialog_scroll = u16::MAX;
    draw(&mut state, 24, 10);
    state.key_push('x');
    let typed = text(&draw(&mut state, 24, 10));
    assert!(
        typed.contains("> *"),
        "typing reveals the masked field again: {typed}"
    );
    assert!(!typed.contains("> x"), "the secret is not drawn");
    state.dialog_scroll = u16::MAX;
    let bottom = text(&draw(&mut state, 24, 10));
    assert!(
        bottom.contains("F9 quits"),
        "instructions remain reachable: {bottom}"
    );
}

#[test]
fn keyboard_dialog_copy_contains_hidden_content_but_never_the_typed_secret() {
    let mut state = ViewState::new();
    state.open_credential("vendor", Some("api"), "KEY");
    state.open_key_entry();
    for character in "never-copy-this-secret".chars() {
        state.key_push(character);
    }
    let copied = state.dialog_text().expect("active dialog");
    assert!(copied.contains("key for vendor"));
    assert!(copied.contains('*'));
    assert!(!copied.contains("never-copy-this-secret"));
    state.close_key_entry();
    state.open_help();
    draw(&mut state, 24, 8);
    let copied = state.dialog_text().expect("help");
    assert!(copied.contains("needs no terminal cooperation"));
    assert!(copied.contains("n / Esc: deny it"));
}

#[test]
fn an_approval_preempts_every_dialog_but_permissions_and_a_key_being_typed() {
    let mut state = approval();
    state.provider_open = true;
    state.auth_open = true;
    state.help_open = true;
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(drawn.contains("approval needed"));
    assert!(!drawn.contains("Esc hides this"));
    state.open_permissions();
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(drawn.contains("permission"));
    assert!(!drawn.contains("approval needed"));
    assert!(
        state.pending_approval.is_some(),
        "the hidden question is retained"
    );
    state.close_permissions();

    // The masked field is the other dialog drawn over a question: what is on screen is what owns
    // the keys, and a key being typed must not be handed to a dialog that reads letters as answers.
    state.open_credential("vendor", Some("api"), "KEY");
    state.open_key_entry();
    assert_eq!(state.active_modal(), Some(Modal::KeyEntry));
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(drawn.contains("Enter stores"), "{drawn}");
    assert!(!drawn.contains("approval needed"));
    state.close_key_entry();
    state.credential_open = false;
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(
        drawn.contains("approval needed"),
        "and the question is back when the field is closed: {drawn}"
    );
}

/// A reason may be the command a reader is about to allow, so it is shown as it was written. The
/// prose wrapper this used collapsed runs of spaces and dropped the ones a line began with, which
/// drew — and copied — `printf 'a    b'` as `printf 'a b'`.
#[test]
fn a_reason_is_drawn_and_copied_exactly_as_it_was_written() {
    let mut state = ViewState::new();
    state.pending_approval = Some(PendingApproval {
        call_id: "audit".into(),
        tool: "bash".into(),
        reason: Some("printf 'a    b'\n    indented\tcolumn".into()),
    });
    let drawn = text(&draw(&mut state, 80, 24));
    let copied = state.dialog_text().expect("the dialog");
    for shown in [&drawn, &copied] {
        assert!(shown.contains("printf 'a    b'"), "{shown}");
        assert!(shown.contains("    indented    column"), "{shown}");
    }
    assert!(!drawn.contains("printf 'a b'"));
}

#[test]
fn literal_wrapping_keeps_spaces_and_never_exceeds_the_width() {
    let rows = |source: &str, width: usize| {
        dialog::wrapped(source, width, Style::default())
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("alpha beta gamma", 10), ["alpha beta", "gamma"]);
    assert_eq!(rows("  two  spaces", 40), ["  two  spaces"]);
    assert_eq!(rows("a    b", 3), ["a", "b"], "a break takes its spaces");
    assert_eq!(
        rows("x\n\ny", 9),
        ["x", "", "", "y"],
        "hard newlines survive and leave a wider paragraph gap"
    );
    assert_eq!(
        rows("a\n\n\nb", 9),
        ["a", "", "", "b"],
        "a run of blanks is one gap, not several"
    );
    assert_eq!(
        rows("\n\na", 9),
        ["a"],
        "leading blanks do not open the dialog"
    );
    assert_eq!(rows("a\n\n", 9), ["a"], "trailing blanks do not close it");
    assert_eq!(rows("", 9), [""]);
    assert_eq!(rows("trailing   ", 20), ["trailing"]);
    assert_eq!(
        rows("abcdefgh", 3),
        ["abc", "def", "gh"],
        "a long word is cut"
    );
    assert_eq!(
        rows("界界界", 4),
        ["界界", "界"],
        "by columns, not characters"
    );
    assert_eq!(
        rows("e\u{301}e\u{301}e\u{301}", 2),
        ["e\u{301}e\u{301}", "e\u{301}"],
        "and never inside a grapheme"
    );
    assert_eq!(
        rows("        deep", 6),
        ["     d", "eep"],
        "an indent leaves a column"
    );
    assert_eq!(
        rows("界", 1),
        ["界"],
        "too wide for the dialog still gets a row"
    );
    // The other direction, across widths: nothing is lost but the spaces a break lands on.
    let source = "one  two   three 界界 four\tfive   six";
    for width in 2..40 {
        let wrapped = rows(source, width);
        for row in &wrapped {
            assert!(row.width() <= width, "{row:?} at {width}");
        }
        let kept: String = wrapped.concat().split_whitespace().collect();
        let original: String = source.split_whitespace().collect();
        assert_eq!(kept, original, "at {width}");
    }
}

/// Unselected rows keep the gutter the marker is drawn in, so choosing does not move the names.
#[test]
fn a_choosers_rows_stay_aligned_under_the_marker() {
    let mut state = ViewState::new();
    state.models = vec!["model-a".into(), "model-b".into(), "model-c".into()];
    state.open_models();
    state.model_selection = 1;
    let drawn = text(&draw(&mut state, 80, 24));
    let column = |needle: &str| {
        drawn
            .lines()
            .find_map(|line| line.find(needle).map(|at| line[..at].width()))
            .unwrap_or_else(|| panic!("{needle} is drawn: {drawn}"))
    };
    assert!(drawn.contains("▶ 2 model-b"));
    assert_eq!(column("1 model-a"), column("2 model-b"));
    assert_eq!(column("3 model-c"), column("2 model-b"));
}

/// A reader halfway down a long reason who presses Tab to choose an answer keeps their place. A
/// different dialog opening is the other direction: that one does start from its top.
#[test]
fn choosing_an_answer_keeps_the_readers_place_in_a_long_reason() {
    let mut state = approval();
    draw(&mut state, 40, 12);
    state.dialog_scroll = 3;
    draw(&mut state, 40, 12);
    assert_eq!(state.dialog_scroll, 3);
    state.approval_selection = 1;
    let chosen = text(&draw(&mut state, 40, 12));
    assert_eq!(
        state.dialog_scroll, 3,
        "the selection moved, the page did not"
    );
    assert!(chosen.contains("Selected: allow once"), "{chosen}");
    draw(&mut state, 44, 12);
    assert_eq!(state.dialog_scroll, 3, "nor does a resize");

    state.open_permissions();
    draw(&mut state, 40, 12);
    assert_eq!(
        state.dialog_scroll, 0,
        "a different dialog starts at its top"
    );
}

#[test]
fn the_hardware_cursor_tracks_the_visible_composer_and_not_a_hidden_one() {
    let mut state = ViewState::new();
    state.input.insert_str("wide 界");
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("terminal");
    terminal.draw(|frame| state.render(frame)).expect("draw");
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor");
    let composer = state.last_composer.expect("composer");
    assert_eq!(cursor.x, composer.x.saturating_add(9));
    assert!(cursor.y >= composer.y && cursor.y < composer.bottom());
    state.open_help();
    terminal
        .draw(|frame| state.render(frame))
        .expect("modal draw");
    assert!(format!("{:?}", terminal.backend()).contains("cursor: false"));
}

#[test]
fn a_cjk_composer_wraps_by_columns_instead_of_losing_characters() {
    let mut state = ViewState::new();
    state.input.insert_str(&"界".repeat(20));
    let buffer = draw(&mut state, 16, 20);
    let shown = buffer
        .content
        .iter()
        .filter(|cell| cell.symbol() == "界")
        .count();
    assert_eq!(shown, 20);
    let layout = state.composer_layout(14);
    for row in layout.rows {
        let width = row
            .prefix
            .chars()
            .chain(row.text.chars())
            .map(|c| c.width().unwrap_or(0))
            .fold(0_usize, usize::saturating_add);
        assert!(width <= 14);
    }
}

#[test]
fn copy_feedback_is_visible_when_a_short_modal_covers_the_status_line() {
    let mut state = ViewState::new();
    state.open_help();
    state.status = "the UI text was not copied: clipboard unavailable".into();
    let drawn = text(&draw(&mut state, 40, 12));
    assert!(drawn.contains("the UI text was not copied"), "{drawn}");
    state.status = "copied 42 characters".into();
    let drawn = text(&draw(&mut state, 40, 12));
    assert!(drawn.contains("copied 42 characters"));
    assert!(
        drawn.contains("Esc closes"),
        "feedback does not remove the exit hint: {drawn}"
    );
    // On a row of its own: painted over the dialog's border it left the border's tail showing.
    let row = drawn
        .lines()
        .find(|line| line.contains("copied 42 characters"))
        .expect("the feedback row");
    assert!(
        row.contains("copied 42 characters   ") && !row.contains(['─', '┐', '┌']),
        "the row is the feedback and nothing of the border: {drawn}"
    );
}

/// A reader on a short terminal who answers too soon has to be told the key was not taken, and the
/// status line that says so is under the dialog.
#[test]
fn a_held_approval_key_is_reported_even_when_the_dialog_covers_the_status_line() {
    let mut state = approval();
    state.status = format!("{HELD_KEY_NOTICE} · pause, then decide the bash call");
    let short = text(&draw(&mut state, 40, 12));
    assert!(short.contains(HELD_KEY_NOTICE), "{short}");
    assert!(
        short.contains("n/Esc deny"),
        "and the answers stay: {short}"
    );
    let tall = text(&draw(&mut state, 80, 24));
    assert_eq!(tall.matches("that key was not").count(), 1, "{tall}");
    assert!(tall.contains("approval needed"), "{tall}");
}

/// The other direction: while the status line can be seen, the result is there and nowhere else.
/// It used to be painted over the title bar of every screen with a dialog open.
#[test]
fn copy_feedback_stays_on_the_status_line_when_the_dialog_leaves_it_visible() {
    let mut state = approval();
    state.label = Some("a session".into());
    state.status = "copied 3 lines".into();
    let drawn = text(&draw(&mut state, 80, 24));
    let rows: Vec<&str> = drawn.lines().collect();
    assert!(
        rows.first().is_some_and(|row| !row.contains("copied")),
        "the title bar is left alone: {drawn}"
    );
    assert!(
        rows.last()
            .is_some_and(|row| row.contains("copied 3 lines")),
        "{drawn}"
    );
    assert_eq!(drawn.matches("copied 3 lines").count(), 1, "{drawn}");
    assert!(
        drawn.contains("approval needed"),
        "the dialog keeps its title"
    );
}

#[test]
fn tiny_screens_keep_footer_content_reachable_and_reveal_a_changed_selection() {
    let mut state = ViewState::new();
    state.models = (0..20).map(|index| format!("model-{index:02}")).collect();
    state.model_open = true;
    state.model_selection = 19;
    let drawn = text(&draw(&mut state, 16, 4));
    assert!(
        drawn.contains("▶ 20 model-19"),
        "even tiny focus is revealed: {drawn}"
    );
    state.dialog_scroll = u16::MAX;
    let end = text(&draw(&mut state, 16, 4));
    assert!(
        end.contains("F6 copy"),
        "the final footer text is reachable: {end}"
    );
    state.model_selection = 0;
    let first = text(&draw(&mut state, 16, 4));
    assert!(
        first.contains("▶ 1 model-00"),
        "changing focus reveals it again: {first}"
    );
}

#[test]
fn combining_characters_and_joined_emoji_are_edited_and_wrapped_as_units() {
    for grapheme in ["e\u{301}", "👩‍💻", "🇬🇧"] {
        let mut input = InputBuffer::with_text(&format!("a{grapheme}b"));
        input.move_home();
        input.move_right();
        input.move_right();
        assert_eq!(
            input.cursor(),
            1_usize.saturating_add(grapheme.chars().count())
        );
        assert!(input.backspace());
        assert_eq!(input.text(), "ab");
        input = InputBuffer::with_text(&format!("a{grapheme}b"));
        input.place_cursor(0, 1);
        assert!(input.delete());
        assert_eq!(input.text(), "ab");
        input = InputBuffer::with_text(&format!("a{grapheme}b"));
        input.place_cursor(0, 2);
        assert_eq!(
            input.cursor(),
            1,
            "a click inside a suffix selects its whole grapheme"
        );
        let rows = wrap_words(&grapheme.repeat(12), 8, 8);
        assert_eq!(
            rows.iter()
                .map(|(text, _, _)| text.as_str())
                .collect::<String>(),
            grapheme.repeat(12)
        );
        assert!(rows.iter().all(|(text, _, _)| text.width() <= 8));
    }
    let mut state = ViewState::new();
    state.input.insert_str("a👩‍💻b");
    state.input.move_left();
    state.input.move_left();
    let buffer = draw(&mut state, 40, 12);
    let caret = buffer
        .content
        .iter()
        .find(|cell| cell.modifier.contains(Modifier::REVERSED))
        .expect("caret");
    assert_eq!(
        caret.symbol(),
        "👩‍💻",
        "the caret does not detach a joined emoji"
    );
}

#[test]
fn completing_unicode_mentions_preserves_both_sides_of_the_draft() {
    for path in ["e\u{301}.rs", "👩‍💻.rs"] {
        let mut state = ViewState::new();
        state.input.insert_str(&format!("read @{path} next"));
        state.input.place_cursor(0, 7);
        state.show_mentions(vec!["completed.rs".into()]);
        assert!(state.accept_mention());
        assert_eq!(state.input.text(), "read completed.rs  next");
    }
}

#[test]
fn copying_wide_screen_text_does_not_insert_continuation_cell_spaces() {
    let mut state = ViewState::new();
    state.input.insert_str("界界👩‍💻");
    draw(&mut state, 40, 12);
    let copied = state.screen_text_for_copy();
    assert!(copied.contains("界界👩‍💻"), "{copied}");
}

#[test]
fn dialog_rectangles_are_contained_even_during_tiny_resizes() {
    for width in 0..=40 {
        for height in 0..=14 {
            let area = Rect::new(2, 3, width, height);
            let bounds = dialog::bounds(area, 78, 1000);
            assert!(bounds.x >= area.x && bounds.y >= area.y);
            assert!(bounds.right() <= area.right() && bounds.bottom() <= area.bottom());
            let mut state = approval();
            draw(&mut state, width, height);
        }
    }
}
