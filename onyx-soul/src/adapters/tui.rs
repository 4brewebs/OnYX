//! Adaptador TUI (Ratatui + Crossterm).
//! Implementa PlatformDriver: poll traduce teclas a Ingress,
//! present dibuja las SurfaceBinding en la terminal.

use std::io::{self, Stdout};

use crossterm::{
    event::{self, Event, KeyCode as CtKeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

use onyx_core::bus::EventBus;
use onyx_core::event::{IngressEvent, Key, KeyCode, Mods, OnyxEvent};
use onyx_core::graph::GraphView;
use onyx_core::surface::{SurfaceBinding, SurfaceId};
use onyx_core::PlatformDriver;

pub struct TuiDriver {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TuiDriver {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn shutdown_inner(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for TuiDriver {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

impl PlatformDriver for TuiDriver {
    fn poll(&mut self, bus: &mut dyn EventBus) {
        // No bloqueante
        if event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                let key = translate_key(code, modifiers);
                bus.push(OnyxEvent::Ingress(IngressEvent::Key(key)));
            }
        }
    }

    fn present(&mut self, graph: &dyn GraphView, surfaces: &[SurfaceBinding]) {
        let _ = self.terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(100)])
                .split(f.size());

            // Ejemplo mínimo: dibuja un bloque por superficie
            for (i, binding) in surfaces.iter().enumerate() {
                let title = format!("Surface {:?}", binding.surface);
                let body = format!(
                    "view={:?} focused={}",
                    binding.view, binding.focused
                );
                // Aquí leerías del graph lo que corresponda a binding.view
                let _ = graph; // evita warning si aún no lo usas

                let p = Paragraph::new(body).block(
                    Block::default().title(title).borders(Borders::ALL),
                );
                // En un layout real repartirías los chunks
                if i == 0 {
                    f.render_widget(p, chunks[0]);
                }
            }
        });
    }
}

fn translate_key(code: CtKeyCode, modifiers: KeyModifiers) -> Key {
    let mut mods = Mods::default();
    if modifiers.contains(KeyModifiers::SHIFT) {
        mods.0 |= Mods::SHIFT;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        mods.0 |= Mods::CTRL;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mods.0 |= Mods::ALT;
    }

    let code = match code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Esc => KeyCode::Esc,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        CtKeyCode::Left => KeyCode::Left,
        CtKeyCode::Right => KeyCode::Right,
        CtKeyCode::F(n) => KeyCode::F(n),
        _ => KeyCode::Char('?'),
    };

    Key { code, mods }
}
