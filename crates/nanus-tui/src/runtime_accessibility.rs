//! Input regressions for modal safety, global exit and pointer-free UI copying.

use super::*;
use crate::runtime::tests::{drawn, key, recording_copier};

fn approve(view: &mut ViewState) {
    apply_approval(
        "audit".into(),
        "bash".into(),
        Some("sandbox exception".into()),
        view,
    );
}

#[test]
fn modified_approval_letters_and_enter_never_grant_a_call() {
    let mut view = ViewState::new();
    approve(&mut view);
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ] {
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('a'),
            KeyCode::Char('n'),
            KeyCode::Enter,
        ] {
            assert!(matches!(
                handle_key(key(code, modifiers), &mut view),
                Outcome::Continue
            ));
            assert!(view.pending_approval.is_some());
            assert!(!view.approval_confirming);
        }
    }
    assert!(
        matches!(
            handle_key(key(KeyCode::Enter, KeyModifiers::NONE), &mut view),
            Outcome::Answer {
                allow: false,
                always: false,
                ..
            }
        ),
        "Enter starts at denial"
    );
}

#[test]
fn a_standing_approval_needs_an_explicit_second_confirmation() {
    let mut view = ViewState::new();
    approve(&mut view);
    assert!(matches!(
        handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE), &mut view),
        Outcome::Continue
    ));
    assert!(view.approval_confirming);
    for code in [KeyCode::Char('a'), KeyCode::Enter] {
        assert!(matches!(
            handle_key(key(code, KeyModifiers::NONE), &mut view),
            Outcome::Continue
        ));
    }
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        assert!(matches!(
            handle_key(key(KeyCode::Char('y'), modifiers), &mut view),
            Outcome::Continue
        ));
    }
    assert!(matches!(
        handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &mut view),
        Outcome::Continue
    ));
    assert!(!view.approval_confirming);
    assert!(
        view.pending_approval.is_some(),
        "cancel returns to the original question"
    );
    handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE), &mut view);
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
}

#[test]
fn approval_choices_have_forward_and_backward_navigation() {
    let mut view = ViewState::new();
    approve(&mut view);
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
fn the_visible_approval_owns_keys_when_provider_or_secret_entry_is_pending() {
    let mut view = ViewState::new();
    view.provider_open = true;
    view.auth_open = true;
    view.key_entry_open = true;
    view.key_input = "private".into();
    approve(&mut view);
    assert_eq!(view.active_modal(), Some(Modal::Approval));
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
    assert_eq!(view.key_input, "private");
    assert!(
        view.provider_open && view.auth_open,
        "background state is not changed"
    );
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
