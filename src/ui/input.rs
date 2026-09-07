use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;

/// What the event loop should do after a key press.
pub enum Action {
    Continue,
    Quit,
    Rescan,
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    // Ctrl+C always quits, even mid-filter.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return Action::Quit;
    }
    if app.filtering {
        match key.code {
            KeyCode::Enter => app.filtering = false,
            KeyCode::Esc => {
                app.set_filter(String::new());
                app.filtering = false;
            }
            KeyCode::Backspace => {
                app.filter_text.pop();
                let text = app.filter_text.clone();
                app.set_filter(text);
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                app.set_filter(String::new());
            }
            KeyCode::Char(c) => {
                app.filter_text.push(c);
                let text = app.filter_text.clone();
                app.set_filter(text);
            }
            _ => {}
        }
        return Action::Continue;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) => Action::Quit,
        (KeyCode::Esc, _) => {
            if app.filter_text.is_empty() {
                Action::Quit
            } else {
                app.set_filter(String::new());
                Action::Continue
            }
        }
        (KeyCode::Char('/'), _) => {
            app.filtering = true;
            Action::Continue
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            app.next();
            Action::Continue
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            app.previous();
            Action::Continue
        }
        (KeyCode::PageDown, _) => {
            app.page_down();
            Action::Continue
        }
        (KeyCode::PageUp, _) => {
            app.page_up();
            Action::Continue
        }
        (KeyCode::Char('g'), _) => {
            app.top();
            Action::Continue
        }
        (KeyCode::Char('G'), _) => {
            app.bottom();
            Action::Continue
        }
        (KeyCode::Char('s'), _) => {
            app.cycle_sort();
            Action::Continue
        }
        (KeyCode::Char('d'), _) => {
            app.delete_selected();
            Action::Continue
        }
        (KeyCode::Char('r'), _) => Action::Rescan,
        _ => Action::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn esc_clears_applied_filter() {
        let mut app = App::new(PathBuf::from("."));
        app.set_filter("big".to_string());
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Esc)),
            Action::Continue
        ));
        assert!(app.filter_text.is_empty());
        assert!(app.filter_regex.is_none());
    }

    #[test]
    fn esc_quits_without_filter() {
        let mut app = App::new(PathBuf::from("."));
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Esc)),
            Action::Quit
        ));
    }

    #[test]
    fn esc_while_typing_clears_and_exits_mode() {
        let mut app = App::new(PathBuf::from("."));
        app.filtering = true;
        app.set_filter("bi".to_string());
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Esc)),
            Action::Continue
        ));
        assert!(!app.filtering);
        assert!(app.filter_text.is_empty());
    }

    #[test]
    fn esc_while_typing_empty_just_exits_mode() {
        let mut app = App::new(PathBuf::from("."));
        app.filtering = true;
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Esc)),
            Action::Continue
        ));
        assert!(!app.filtering);
    }

    #[test]
    fn d_deletes_selected_target() {
        let root = std::env::temp_dir().join("cargo-storage-test-input-delete");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj")]);
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Char('d'))),
            Action::Continue
        ));
        assert!(!root.join("proj/target").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn d_while_filtering_types_instead_of_deleting() {
        let root = std::env::temp_dir().join("cargo-storage-test-input-delete-filter");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj")]);
        app.filtering = true;
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::Char('d'))),
            Action::Continue
        ));
        assert!(root.join("proj/target").exists());
        assert_eq!(app.filter_text, "d");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn page_keys_jump_one_screen() {
        let mut app = App::new(PathBuf::from("."));
        let projects: Vec<PathBuf> = (0..30)
            .map(|i| PathBuf::from(format!("proj-{i:02}")))
            .collect();
        app.set_discovered(projects);
        app.finish_scan(None);
        app.page_len = 10;
        app.table_state.select(Some(0));
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::PageDown)),
            Action::Continue
        ));
        assert_eq!(app.table_state.selected(), Some(10));
        assert!(matches!(
            handle_key(&mut app, key(KeyCode::PageUp)),
            Action::Continue
        ));
        assert_eq!(app.table_state.selected(), Some(0));
    }
}
