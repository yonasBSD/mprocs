use tokio::sync::mpsc::UnboundedReceiver;

use super::views::term::term_view;
use crate::color;
use crate::mprocs::app::{ClientHandle, ClientId};
use crate::term::grid::BorderType;
use crate::{
  error::ResultLogger,
  kernel::kernel_message::{
    KernelCommand, KernelQuery, KernelQueryResponse, SharedVt, TaskContext,
  },
  kernel::task::{
    TaskCmd, TaskDef, TaskId, TaskNotification, TaskNotify, TaskStatus,
  },
  protocol::{CltToSrv, SrvToClt},
  server::server_message::ServerMessage,
  term::{
    Color, Grid, Size, TermEvent,
    attrs::Attrs,
    grid::Rect,
    key::{KeyCode, KeyEventKind, KeyMods},
    scroll_offset,
  },
};

struct ConsoleTaskEntry {
  id: TaskId,
  path: String,
  status: TaskStatus,
  vt: Option<SharedVt>,
}

struct Console {
  task_context: TaskContext,
  receiver: UnboundedReceiver<TaskCmd>,
  clients: Vec<ClientHandle>,
  grid: Grid,
  screen_size: Size,

  tasks: Vec<ConsoleTaskEntry>,
  selected: usize,
}

impl Console {
  async fn run(mut self) {
    self.task_context.send(KernelCommand::ListenTaskUpdates);
    self.refresh_tasks().await;

    let mut render_needed = true;
    let mut command_buf = Vec::new();

    loop {
      if render_needed && !self.clients.is_empty() {
        self.render().await;
        render_needed = false;
      }

      if self.receiver.recv_many(&mut command_buf, 512).await == 0 {
        break;
      }
      for cmd in command_buf.drain(..) {
        if self.handle_cmd(cmd).await {
          render_needed = true;
        }
      }
    }
  }

  async fn handle_cmd(&mut self, cmd: TaskCmd) -> bool {
    match cmd {
      TaskCmd::Msg(msg) => {
        let msg = match msg.downcast::<ServerMessage>() {
          Ok(server_msg) => return self.handle_server_msg(*server_msg).await,
          Err(msg) => msg,
        };
        if let Ok(n) = msg.downcast::<TaskNotification>() {
          return self.handle_notification(n.from, n.notify);
        }
        false
      }
      _ => false,
    }
  }

  fn handle_notification(&mut self, from: TaskId, notify: TaskNotify) -> bool {
    match notify {
      TaskNotify::Added(path, status) => {
        let path = path
          .map(|p| p.to_string())
          .unwrap_or_else(|| format!("<task:{}>", from.0));
        self.tasks.push(ConsoleTaskEntry {
          id: from,
          path,
          status,
          vt: None,
        });
        true
      }
      TaskNotify::Started => {
        if let Some(entry) = self.tasks.iter_mut().find(|t| t.id == from) {
          entry.status = TaskStatus::Running;
        }
        true
      }
      TaskNotify::Stopped(_) => {
        if let Some(entry) = self.tasks.iter_mut().find(|t| t.id == from) {
          entry.status = TaskStatus::Down;
        }
        true
      }
      TaskNotify::ScreenChanged(vt) => {
        if let Some(entry) = self.tasks.iter_mut().find(|t| t.id == from) {
          entry.vt = vt;
        }
        true
      }
      TaskNotify::Removed => {
        self.tasks.retain(|t| t.id != from);
        if self.selected >= self.tasks.len() && !self.tasks.is_empty() {
          self.selected = self.tasks.len() - 1;
        }
        true
      }
      _ => false,
    }
  }

  async fn handle_server_msg(&mut self, msg: ServerMessage) -> bool {
    match msg {
      ServerMessage::ClientConnected { handle } => {
        self.clients.push(handle);
        self.update_screen_size();
        true
      }
      ServerMessage::ClientDisconnected { client_id } => {
        self.clients.retain(|c| c.id != client_id);
        self.update_screen_size();
        true
      }
      ServerMessage::ClientMessage { client_id, msg } => match msg {
        CltToSrv::Key(event) => self.handle_key(client_id, event).await,
        CltToSrv::Init { width, height } => {
          self.screen_size = Size { width, height };
          self.grid.set_size(self.screen_size);
          true
        }
        CltToSrv::Rpc(_) => false,
      },
    }
  }

  async fn handle_key(
    &mut self,
    client_id: ClientId,
    event: TermEvent,
  ) -> bool {
    let key = match event {
      TermEvent::Key(k) if k.kind != KeyEventKind::Release => k,
      _ => return false,
    };

    match key.code {
      KeyCode::Char('j') | KeyCode::Down if key.mods == KeyMods::NONE => {
        self.move_selection(1);
        true
      }
      KeyCode::Char('k') | KeyCode::Up if key.mods == KeyMods::NONE => {
        self.move_selection(-1);
        true
      }
      KeyCode::Char('q') if key.mods == KeyMods::NONE => {
        if let Some(client) =
          self.clients.iter_mut().find(|c| c.id == client_id)
        {
          let _ = client.sender.send(SrvToClt::Quit).await;
        }
        true
      }
      _ => false,
    }
  }

  fn move_selection(&mut self, delta: i32) {
    if self.tasks.is_empty() {
      return;
    }
    let len = self.tasks.len() as i32;
    let new = (self.selected as i32 + delta).rem_euclid(len);
    self.selected = new as usize;
  }

  async fn refresh_tasks(&mut self) {
    let rx = self.task_context.query(KernelQuery::ListTasks(None));
    if let Ok(KernelQueryResponse::TaskList(list)) = rx.await {
      self.tasks = list
        .into_iter()
        .map(|t| ConsoleTaskEntry {
          id: t.id,
          path: t
            .path
            .map(|p| p.to_string())
            .unwrap_or_else(|| format!("<task:{}>", t.id.0)),
          status: t.status,
          vt: None,
        })
        .collect();
      if self.selected >= self.tasks.len() && !self.tasks.is_empty() {
        self.selected = self.tasks.len() - 1;
      }
    }
  }

  fn update_screen_size(&mut self) {
    if let Some(client) = self.clients.first() {
      self.screen_size = client.size();
      self.grid.set_size(self.screen_size);
    }
  }

  async fn render(&mut self) {
    let grid = &mut self.grid;
    grid.erase_all(Attrs::default());
    grid.cursor_pos = None;

    let area = Rect::new(0, 0, self.screen_size.width, self.screen_size.height);
    if area.width < 4 || area.height < 3 {
      return;
    }

    let (title_row, area) = area.split_h(1);
    let (area, help_row) = area.split_h(area.height - 1);
    let (procs_block, term_block) = area.split_v(30);

    // Title bar
    let logo_attrs = Attrs::default()
      .fg(Color::BLACK)
      .bg(color!("#69e8ff"))
      .set_bold(true);
    grid.draw_text(title_row, " dekit ", logo_attrs);
    let bar_attrs = Attrs::default()
      .fg(color!("#69e8ff"))
      .bg(Color::WHITE)
      .set_bold(true);
    grid.draw_line(title_row.move_left(7), "\u{e0bc} ", bar_attrs);

    // Task list
    grid.draw_block(
      procs_block,
      &BorderType::Plain.chars(),
      Attrs::default().fg(color!("#666666")),
    );
    grid.draw_text(
      procs_block.move_left(1).move_right(-2),
      " Processes ",
      Attrs::default().fg(color!("#666666")),
    );
    if self.tasks.is_empty() {
      let attrs = Attrs::default().fg(Color::Idx(245));
      grid.draw_text(procs_block.inner((1, 2)), "No tasks", attrs);
    } else {
      let area = procs_block.inner(1);
      let max_rows = area.height as usize;
      let start = scroll_offset(self.selected, self.tasks.len(), max_rows);

      for (i, task) in self.tasks.iter().enumerate().skip(start).take(max_rows)
      {
        let Some(row) = area.row((i - start) as u16) else {
          break;
        };
        let is_selected = i == self.selected;
        let bg = if is_selected {
          Color::Idx(236)
        } else {
          Color::Default
        };

        let (status_col, path_col) = row.split_v(2);

        let (status_char, status_color) = match task.status {
          TaskStatus::Running => ("●", Color::GREEN),
          TaskStatus::Down => ("○", Color::RED),
        };
        grid.draw_line(
          status_col,
          status_char,
          Attrs::default().fg(status_color).bg(bg),
        );
        grid.draw_line(path_col, &task.path, Attrs::default().bg(bg));
      }
    }

    // Terminal
    let term_block_fg = color!("#bee6f4");
    grid.draw_block(
      term_block,
      &BorderType::Thick.chars(),
      Attrs::default().fg(term_block_fg),
    );
    grid.draw_text(
      term_block.move_left(1).move_right(-2),
      " Terminal ",
      Attrs::default().fg(term_block_fg),
    );
    if let Some(vt) = self.tasks.get(self.selected).and_then(|t| t.vt.as_ref())
    {
      term_view(grid, term_block.inner(1), vt);
    }

    // Bottom help bar
    let help_bg = Attrs::default();
    grid.fill_area(help_row, ' ', help_bg);
    let bindings: &[(&str, &str)] =
      &[("`", "leader"), ("C-h/j/k/l", "select pane")];
    let mut cursor = Rect::new(help_row.x + 1, help_row.y, help_row.width, 1);
    let key_attrs = Attrs::default().fg(color!("#7da8e8")).set_bold(true);
    let desc_attrs = Attrs::default().fg(color!("#dddddd"));
    let sep_attrs = Attrs::default().fg(color!("#888888"));
    for (i, (key, desc)) in bindings.into_iter().enumerate() {
      if i > 0 {
        let used = grid.draw_text(cursor, " \u{00b7} ", sep_attrs);
        cursor.x = used.right();
      }

      let used = grid.draw_text(cursor, &format!("{}", key), key_attrs);
      cursor.x = used.right();
      let used = grid.draw_text(cursor, &format!(" {}", desc), desc_attrs);
      cursor.x = used.right();
    }

    // Send diffs to clients
    for client in &mut self.clients {
      let mut out = String::new();
      client.differ.diff(&mut out, grid).log_ignore();
      let _ = client.sender.send(SrvToClt::Print(out)).await;
      let _ = client.sender.send(SrvToClt::Flush).await;
    }
  }
}

pub fn create_console_task(pc: &TaskContext) -> TaskId {
  pc.spawn_async(
    TaskDef {
      status: TaskStatus::Running,
      ..Default::default()
    },
    |pc, receiver| async move {
      log::debug!("Creating console task (id: {})", pc.task_id.0);
      let app = Console {
        task_context: pc,
        receiver,
        clients: Vec::new(),
        grid: Grid::new(
          Size {
            width: 80,
            height: 24,
          },
          0,
        ),
        screen_size: Size {
          width: 80,
          height: 24,
        },
        tasks: Vec::new(),
        selected: 0,
      };
      app.run().await;
    },
  )
}
