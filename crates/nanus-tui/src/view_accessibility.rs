//! Regression tests for small screens, visible focus and whole-frame monochrome styling.

use super::*;
use ratatui::Terminal;
use ratatui::backend::{Backend as _, TestBackend};
use ratatui::style::Color;

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

#[test]
fn monochrome_covers_every_dialog_and_markdown_not_just_transcript_roles() {
    let mut state = approval();
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
    for modal in [
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
    ] {
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
fn an_approval_preempts_all_other_dialogs_visually() {
    let mut state = approval();
    state.provider_open = true;
    state.key_entry_open = true;
    state.auth_open = true;
    state.help_open = true;
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(drawn.contains("approval needed"));
    assert!(!drawn.contains("api key"));
    assert!(!drawn.contains("Esc hides this"));
    state.open_permissions();
    let drawn = text(&draw(&mut state, 80, 24));
    assert!(drawn.contains("permission"));
    assert!(!drawn.contains("approval needed"));
    assert!(
        state.pending_approval.is_some(),
        "the hidden question is retained"
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
