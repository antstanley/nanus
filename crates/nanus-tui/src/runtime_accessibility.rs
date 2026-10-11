//! Input regressions for modal safety, global exit and pointer-free UI copying.

use super::*;
use crate::runtime::tests::{drawn, key, pause, recording_copier};

fn approve(view: &mut ViewState) {
    apply_approval(
        "audit".into(),
        "bash".into(),
        Some("sandbox exception".into()),
        view,
    );
}

/// One plain key, as a reader would press it.
fn press(code: KeyCode, view: &mut ViewState) -> Outcome {
    handle_key(key(code, KeyModifiers::NONE), view)
}

/// Types `text` the way a sentence or a paste arrives: one key after another, a fraction of the
/// settling time apart. Returns whether any key was taken as an answer.
fn typed(text: &str, view: &mut ViewState) -> bool {
    let mut answered = false;
    for character in text.chars() {
        view.now = view
            .now
            .saturating_add(core::time::Duration::from_millis(120));
        let code = match character {
            '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab,
            other => KeyCode::Char(other),
        };
        answered |= matches!(press(code, view), Outcome::Answer { .. });
    }
    answered
}

#[test]
fn modified_approval_letters_and_enter_never_grant_a_call() {
    let mut view = ViewState::new();
    approve(&mut view);
    // Settled first, and shown to be: the chords below are refused for being chords, not because
    // the question was still too new to take any key.
    pause(&mut view);
    press(KeyCode::Tab, &mut view);
    assert_eq!(view.approval_selection, 1, "the dialog is taking keys");
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        // What a terminal speaking the enhanced keyboard protocol reports, and an ordinary one
        // never does: naming only Ctrl and Alt left every one of these meaning the bare letter.
        KeyModifiers::SUPER,
        KeyModifiers::HYPER,
        KeyModifiers::META,
        KeyModifiers::SUPER | KeyModifiers::SHIFT,
    ] {
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('a'),
            KeyCode::Char('n'),
            KeyCode::Enter,
        ] {
            assert!(
                matches!(
                    handle_key(key(code, modifiers), &mut view),
                    Outcome::Continue
                ),
                "{modifiers:?}+{code:?}"
            );
            assert!(view.pending_approval.is_some());
            assert!(!view.approval_confirming);
        }
    }
    // Shift+Enter is the composer's newline, so with "allow once" selected it is still not a yes.
    assert!(matches!(
        handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT), &mut view),
        Outcome::Continue
    ));
    // The other direction: the same letter with only Shift held is the letter, and the plain
    // Enter is the selection.
    assert!(matches!(
        handle_key(key(KeyCode::Char('Y'), KeyModifiers::SHIFT), &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
    assert!(matches!(
        press(KeyCode::Enter, &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
    press(KeyCode::Up, &mut view);
    assert!(
        matches!(
            press(KeyCode::Enter, &mut view),
            Outcome::Answer {
                allow: false,
                always: false,
                ..
            }
        ),
        "Enter on the first choice is denial"
    );
}

#[test]
fn a_new_approval_starts_at_denial_and_takes_no_key_until_the_keyboard_is_quiet() {
    let mut view = ViewState::new();
    approve(&mut view);
    assert_eq!(view.approval_selection, 0, "Enter starts at denial");
    for code in [
        KeyCode::Char('y'),
        KeyCode::Char('a'),
        KeyCode::Char('n'),
        KeyCode::Enter,
        KeyCode::Tab,
    ] {
        assert!(matches!(press(code, &mut view), Outcome::Continue));
        assert!(view.pending_approval.is_some());
        assert!(!view.approval_confirming);
        assert_eq!(view.approval_selection, 0);
    }
    assert!(
        view.status.contains("pause"),
        "a reader who meant it is told to press again: {}",
        view.status
    );
    // `Esc` is the exception, in the one direction that needs no protecting: it refuses at once.
    assert!(matches!(
        press(KeyCode::Esc, &mut view),
        Outcome::Answer { allow: false, .. }
    ));
    // And the other direction: after a pause the same keys are answers.
    pause(&mut view);
    assert!(matches!(
        press(KeyCode::Enter, &mut view),
        Outcome::Answer {
            allow: false,
            always: false,
            ..
        }
    ));
    assert!(matches!(
        press(KeyCode::Char('y'), &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
}

/// The dialog opens over a composer that is live during a turn, and its answers are letters. A
/// sentence in flight when the question arrives used to answer it: `okay` and `today` were a
/// standing grant, and the mention gesture — Tab, then Enter — allowed the call once.
#[test]
fn prose_in_flight_cannot_answer_an_arriving_approval() {
    for sentence in [
        "okay",
        "today",
        "play it",
        "any",
        "xyz",
        "yes",
        "no",
        "ay",
        "\t\n",
        "sk-proj-a81Xy02",
    ] {
        let mut view = ViewState::new();
        assert!(!typed("ple", &mut view));
        assert_eq!(
            view.input.text(),
            "ple",
            "the composer was being typed into"
        );
        approve(&mut view);
        assert!(
            !typed(sentence, &mut view),
            "{sentence:?} answered a question nobody read"
        );
        assert!(view.pending_approval.is_some(), "{sentence:?}");
        assert_eq!(view.input.text(), "ple", "the dialog owns the keys");
        // Nothing typed is left half-agreed either. A pause, and Enter is still a denial.
        pause(&mut view);
        if view.approval_confirming {
            // `ay`: the confirmation is on screen, unconfirmed, and any key but `y` withdraws it.
            press(KeyCode::Enter, &mut view);
            pause(&mut view);
        }
        assert!(
            matches!(
                press(KeyCode::Enter, &mut view),
                Outcome::Answer { allow: false, .. }
            ),
            "{sentence:?} moved the selection off denial"
        );
    }
}

/// A reader who paused and went on typing has not read the question either. A key that means
/// nothing to the dialog closes it to keys again, and a letter cannot be both steps of a grant.
#[test]
fn a_sentence_resumed_after_a_pause_is_not_a_standing_grant() {
    for sentence in ["okay", "today", "may", "any", "lay"] {
        let mut view = ViewState::new();
        approve(&mut view);
        pause(&mut view);
        let mut granted = false;
        for character in sentence.chars() {
            view.now = view
                .now
                .saturating_add(core::time::Duration::from_millis(120));
            granted |= matches!(
                press(KeyCode::Char(character), &mut view),
                Outcome::Answer { always: true, .. }
            );
        }
        assert!(!granted, "{sentence:?} granted every later call");
    }
}

#[test]
fn a_standing_approval_needs_an_explicit_second_confirmation() {
    let mut view = ViewState::new();
    approve(&mut view);
    pause(&mut view);
    assert!(matches!(
        press(KeyCode::Char('a'), &mut view),
        Outcome::Continue
    ));
    assert!(view.approval_confirming);
    // The confirmation is a new question, so the `y` straight after the `a` is not its answer.
    assert!(matches!(
        press(KeyCode::Char('y'), &mut view),
        Outcome::Continue
    ));
    assert!(view.approval_confirming, "held, not withdrawn");
    pause(&mut view);
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SUPER,
    ] {
        assert!(matches!(
            handle_key(key(KeyCode::Char('y'), modifiers), &mut view),
            Outcome::Continue
        ));
    }
    assert!(view.approval_confirming, "a chord is not a key here at all");
    // Every other key takes the first step back: repeating `a`, Enter, `n`, a stray letter.
    for code in [
        KeyCode::Char('a'),
        KeyCode::Enter,
        KeyCode::Char('n'),
        KeyCode::Char('x'),
        KeyCode::Esc,
    ] {
        view.approval_confirming = true;
        pause(&mut view);
        assert!(matches!(press(code, &mut view), Outcome::Continue));
        assert!(!view.approval_confirming, "{code:?} withdraws");
        assert_eq!(view.approval_selection, 0);
        assert!(
            view.pending_approval.is_some(),
            "and returns to the original question"
        );
    }
    // Withdrawing needs no pause, because it only refuses.
    pause(&mut view);
    press(KeyCode::Char('a'), &mut view);
    assert!(view.approval_confirming);
    press(KeyCode::Char('n'), &mut view);
    assert!(!view.approval_confirming);

    pause(&mut view);
    press(KeyCode::Char('a'), &mut view);
    pause(&mut view);
    assert!(matches!(
        handle_key(key(KeyCode::Char('Y'), KeyModifiers::SHIFT), &mut view),
        Outcome::Answer {
            allow: true,
            always: true,
            ..
        }
    ));
    approve(&mut view);
    assert!(
        !view.approval_confirming,
        "a new question cannot inherit consent"
    );
    assert_eq!(view.approval_selection, 0);
    assert!(
        matches!(press(KeyCode::Char('y'), &mut view), Outcome::Continue),
        "nor the quiet the last one had earned"
    );
}

#[test]
fn approval_choices_have_forward_and_backward_navigation() {
    let mut view = ViewState::new();
    approve(&mut view);
    pause(&mut view);
    handle_key(key(KeyCode::Tab, KeyModifiers::NONE), &mut view);
    assert_eq!(view.approval_selection, 1);
    assert!(matches!(
        handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
    // Shift+Tab remains the documented permission shortcut; arrows go backwards in approvals.
    handle_key(key(KeyCode::Left, KeyModifiers::NONE), &mut view);
    assert_eq!(view.approval_selection, 0);
    handle_key(key(KeyCode::Up, KeyModifiers::NONE), &mut view);
    assert_eq!(view.approval_selection, 2);
    assert!(matches!(
        handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
        Outcome::Continue
    ));
    assert!(view.approval_confirming);
}

#[test]
fn the_visible_approval_owns_keys_when_a_provider_or_authorization_is_pending() {
    let mut view = ViewState::new();
    view.provider_open = true;
    view.auth_open = true;
    approve(&mut view);
    assert_eq!(view.active_modal(), Some(Modal::Approval));
    pause(&mut view);
    assert!(matches!(
        handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
        Outcome::Answer { allow: false, .. }
    ));
    assert!(matches!(
        handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE), &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
    assert!(
        view.provider_open && view.auth_open,
        "background state is not changed"
    );
}

/// The masked field is the one dialog an approval does not take the keyboard from. A provider can
/// be changed while a turn runs, so a question can arrive halfway through a key being typed or
/// pasted — and when the question outranked the field, `sk-proj-a81Xy02` was a standing grant and
/// left `sk-02` behind the mask to be stored.
#[test]
fn a_key_being_typed_is_never_taken_by_an_arriving_approval() {
    let mut view = ViewState::new();
    view.key_entry_open = true;
    assert!(!typed("sk-", &mut view));
    approve(&mut view);
    assert_eq!(view.active_modal(), Some(Modal::KeyEntry));
    assert!(
        view.status.contains("waiting for your decision"),
        "the reader is told a question is waiting: {}",
        view.status
    );
    // Slowly as well as quickly: a pause in the middle of a key is not an answer either.
    assert!(!typed("proj-a81X", &mut view));
    pause(&mut view);
    assert!(!typed("y02", &mut view));
    assert_eq!(view.key_input, "sk-proj-a81Xy02", "every character arrived");
    assert!(view.pending_approval.is_some());
    assert!(!view.approval_confirming);

    // The field closes and the question is the dialog — but not answered by the key that closed
    // it, nor by one that follows on its heels.
    assert!(matches!(press(KeyCode::Esc, &mut view), Outcome::Continue));
    assert_eq!(view.active_modal(), Some(Modal::Approval));
    assert!(matches!(
        press(KeyCode::Char('y'), &mut view),
        Outcome::Continue
    ));
    pause(&mut view);
    assert!(matches!(
        press(KeyCode::Char('y'), &mut view),
        Outcome::Answer {
            allow: true,
            always: false,
            ..
        }
    ));
}

#[test]
fn global_quit_works_in_every_modal_search_and_queue_editor() {
    for modal in [
        Modal::Permissions,
        Modal::Approval,
        Modal::KeyEntry,
        Modal::Credential,
        Modal::Authorization,
        Modal::Provider,
        Modal::Context,
        Modal::Models,
        Modal::Effort,
        Modal::Help,
        Modal::Queue,
    ] {
        let mut view = ViewState::new();
        match modal {
            Modal::Permissions => view.permission_open = true,
            Modal::Approval => approve(&mut view),
            Modal::KeyEntry => view.key_entry_open = true,
            Modal::Credential => view.credential_open = true,
            Modal::Authorization => view.auth_open = true,
            Modal::Provider => view.provider_open = true,
            Modal::Context => {
                view.context_dialog =
                    Some(crate::context::ContextDialog::new(view.context_mode, None));
            }
            Modal::Models => view.model_open = true,
            Modal::Effort => view.effort_open = true,
            Modal::Help => view.help_open = true,
            Modal::Queue => view.queue_open = true,
        }
        assert!(matches!(
            handle_key(key(KeyCode::F(9), KeyModifiers::NONE), &mut view),
            Outcome::Quit
        ));
        for letter in ['q', 'Q', 'd', 'D'] {
            assert!(
                matches!(
                    handle_key(key(KeyCode::Char(letter), KeyModifiers::CONTROL), &mut view),
                    Outcome::Quit
                ),
                "{modal:?}: Ctrl+{letter}"
            );
        }
    }
    let mut search = ViewState::new();
    search.input.search_start();
    assert!(matches!(
        handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL), &mut search),
        Outcome::Quit
    ));
    let mut edit = ViewState::new();
    edit.queue.push("draft".into());
    edit.open_queue();
    edit.begin_queue_edit();
    assert!(matches!(
        handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL), &mut edit),
        Outcome::Quit
    ));
}

#[test]
fn keyboard_ui_copy_includes_hidden_help_without_answering_an_approval() {
    let mut view = ViewState::new();
    view.open_help();
    drawn(&mut view, 24, 8);
    for pressed in [
        key(KeyCode::F(6), KeyModifiers::NONE),
        key(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
    ] {
        let Outcome::CopyText(text) = handle_key(pressed, &mut view) else {
            panic!("explicit copy");
        };
        assert!(text.contains("needs no terminal cooperation"));
        assert!(text.contains("n / Esc: deny it"));
    }
    approve(&mut view);
    let Outcome::CopyText(text) = handle_key(key(KeyCode::F(6), KeyModifiers::NONE), &mut view)
    else {
        panic!("copy approval");
    };
    assert!(text.contains("sandbox exception"));
    assert!(view.pending_approval.is_some());
    assert!(!view.approval_confirming);
    assert_eq!(view.approval_selection, 0);
}

#[test]
fn authorization_url_and_code_are_copyable_after_hiding_the_note() {
    let mut view = ViewState::new();
    view.open_auth(
        "vendor",
        Some("plan"),
        "https://example.org/authorize",
        "CODE-123",
    );
    for hidden in [false, true] {
        if hidden {
            view.hide_auth();
        }
        for (pressed, expected) in [
            (
                key(KeyCode::F(7), KeyModifiers::NONE),
                "https://example.org/authorize",
            ),
            (key(KeyCode::F(8), KeyModifiers::NONE), "CODE-123"),
            (
                key(
                    KeyCode::Char('U'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                "https://example.org/authorize",
            ),
            (
                key(
                    KeyCode::Char('Y'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                "CODE-123",
            ),
        ] {
            let Outcome::CopyText(text) = handle_key(pressed, &mut view) else {
                panic!("copy authorization");
            };
            assert_eq!(text, expected);
        }
    }
    view.close_auth();
    assert!(
        matches!(handle_key(key(KeyCode::F(8), KeyModifiers::NONE), &mut view),
        Outcome::CopyText(text) if text.is_empty())
    );
}

#[test]
fn explicit_copy_reports_errors_and_does_not_change_the_active_dialog() {
    let mut view = ViewState::new();
    approve(&mut view);
    for refused in [false, true] {
        let (copied, send) = recording_copier(refused);
        nanus_kernel::runtime::block_on_local(copy_ui_text("instructions", &send, &mut view));
        assert!(view.pending_approval.is_some());
        assert_eq!(view.approval_selection, 0);
        if refused {
            assert!(view.status.contains("not copied"));
            assert!(copied.borrow().is_empty());
        } else {
            assert_eq!(copied.borrow().as_str(), "instructions");
            assert!(view.status.starts_with("copied"));
        }
    }
    let (copied, send) = recording_copier(false);
    nanus_kernel::runtime::block_on_local(copy_ui_text("", &send, &mut view));
    assert!(view.status.contains("no UI text"));
    assert!(copied.borrow().is_empty());
}

#[test]
fn paging_a_long_modal_changes_only_its_reading_position() {
    let mut view = ViewState::new();
    approve(&mut view);
    drawn(&mut view, 40, 12);
    let offset = view.scroll_offset;
    handle_key(key(KeyCode::PageDown, KeyModifiers::NONE), &mut view);
    assert!(view.dialog_scroll > 0);
    assert_eq!(view.approval_selection, 0);
    assert_eq!(view.scroll_offset, offset);
    handle_key(key(KeyCode::Home, KeyModifiers::NONE), &mut view);
    assert_eq!(view.dialog_scroll, 0);
}

#[test]
fn backwards_tab_navigates_choosers_without_opening_permissions() {
    for modal in [Modal::Models, Modal::Effort, Modal::Provider] {
        for pressed in [
            key(KeyCode::BackTab, KeyModifiers::SHIFT),
            key(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let mut view = ViewState::new();
            view.models = vec!["a".into(), "b".into()];
            view.effort_levels = view.models.clone();
            view.providers = vec![
                ProviderChoice {
                    provider: "a".into(),
                    plan: "api".into(),
                    label: "a".into(),
                    env: "KEY".into(),
                    refused: None,
                },
                ProviderChoice {
                    provider: "b".into(),
                    plan: "api".into(),
                    label: "b".into(),
                    env: "KEY".into(),
                    refused: None,
                },
            ];
            view.model_open = modal == Modal::Models;
            view.effort_open = modal == Modal::Effort;
            view.provider_open = modal == Modal::Provider;
            handle_key(pressed, &mut view);
            let selected = match modal {
                Modal::Models => view.model_selection,
                Modal::Effort => view.effort_selection,
                _ => view.provider_selection,
            };
            assert_eq!(selected, 1, "backwards wraps to the final choice");
            assert!(!view.permission_open);
        }
    }
}

#[test]
fn a_recordings_exit_hint_agrees_with_the_global_keys() {
    let hint = recording_status();
    assert!(hint.contains("Ctrl+Q quits"));
    assert!(hint.contains("Ctrl+C copies"));
    assert!(!hint.contains("Ctrl-C quits"));
}

#[test]
fn preferences_select_complete_palettes_and_mouse_capture() {
    assert_eq!(configured_theme(TuiTheme::Terminal), Theme::default());
    assert_eq!(configured_theme(TuiTheme::Light), Theme::light());
    assert_eq!(configured_theme(TuiTheme::Dark), Theme::dark());
    assert_eq!(
        configured_theme(TuiTheme::HighContrast),
        Theme::monochrome()
    );
    let default = NanusConfig::default();
    assert!(default.tui_mouse);
    assert_eq!(default.tui_theme, TuiTheme::Terminal);
}
