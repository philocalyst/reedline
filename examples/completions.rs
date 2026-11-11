// Create a reedline object with tab completions support
// cargo run --example completions
//
// "t" [Tab] types the start the completions share, or selects the first one.
// [Tab] again moves to the next one, [Esc] puts back what you typed, and
// [Enter] takes the selected one.
//
// Completions show below the line as you type; remove
// `with_passive_completions` to see them only after a [Tab].

use reedline::{
    default_emacs_keybindings, DefaultCompleter, DefaultPrompt, EditCommand, Emacs, KeyCode,
    KeyModifiers, Keybindings, Reedline, ReedlineEvent, ReedlineMenu, Signal,
};
use std::io;

fn add_menu_keybindings(keybindings: &mut Keybindings) {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
}

fn main() -> io::Result<()> {
    let commands = vec![
        "test".into(),
        "clear".into(),
        "exit".into(),
        "history 1".into(),
        "history 2".into(),
        "logout".into(),
        "login".into(),
        "hello world".into(),
        "hello world reedline".into(),
        "hello world something".into(),
        "hello world another".into(),
        "hello world 1".into(),
        "hello world 2".into(),
        "hello another very large option for hello word that will force one column".into(),
        "this is the reedline crate".into(),
        "abaaabas".into(),
        "abaaacas".into(),
        "ababac".into(),
        "abacaxyc".into(),
        "abadarabc".into(),
    ];

    let completer = Box::new(DefaultCompleter::new_with_wordlen(commands, 2));

    let mut keybindings = default_emacs_keybindings();
    add_menu_keybindings(&mut keybindings);

    let edit_mode = Box::new(Emacs::new(keybindings));

    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::new("completion_menu"))
        .with_passive_completions(true)
        .with_panel_rows(6)
        .with_edit_mode(edit_mode);

    let prompt = DefaultPrompt::default();

    loop {
        let sig = line_editor.read_line(&prompt)?;
        match sig {
            Signal::Success(buffer) => {
                println!("We processed: {buffer}");
            }
            Signal::CtrlD | Signal::CtrlC(_) => {
                println!("\nAborted!");
                break Ok(());
            }
            _ => {}
        }
    }
}
