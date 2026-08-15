-- rote, from inside nvim.
--
-- A front end over the same localhost protocol `rote watch` renders from: read
-- the event stream, draw the snapshot, send verbs. The daemon is still the only
-- thing that classifies anything — this plugin cannot mark a hunk typed, and
-- there is deliberately no verb by which it could.
--
-- Requires nvim >= 0.10 and `rote` on PATH. No plugin dependencies.

local config = require("rote.config")

local M = {}

--- Pinned against the daemon's `state::WIRE_VERSION`. A Rust test asserts these
--- two integers are equal, which is the only thing standing between a protocol
--- change and a plugin that silently misreads it.
M.WIRE_VERSION = 1

local state = {
  cfg = nil,
  stream = nil,
  snapshot = nil,
  status = nil,
  started = false,
}

function M.snapshot()
  return state.snapshot
end

function M.config()
  return state.cfg or config.defaults
end

local function redraw()
  require("rote.ui").render(state.snapshot, state.status)
end

local function on_event(ev)
  local ui = require("rote.ui")
  local jump = require("rote.jump")

  if state.stream then
    state.stream:mark_alive()
  end

  if ev.type == "snapshot" then
    if ev.wire_version and ev.wire_version ~= M.WIRE_VERSION then
      state.status = ("wire version %s, expected %s — update rote or the plugin"):format(
        ev.wire_version,
        M.WIRE_VERSION
      )
      redraw()
      M.stop()
      return
    end
    state.snapshot = ev
    state.status = nil
    redraw()
    jump.follow(ev.active, state.cfg)
  elseif ev.type == "notice" then
    if ev.level == "warn" then
      vim.notify("rote: " .. (ev.text or ""), vim.log.levels.WARN)
    end
  elseif ev.type == "closed" then
    state.status = ev.terminal == "done" and "session closed"
      or ev.terminal == "aborted" and "session aborted"
      or "the session ended"
    state.snapshot = nil
    redraw()
    jump.sign(nil)
    M.stop()
  end
  -- `heartbeat` is deliberately unhandled: it exists to prove the socket is
  -- alive, which `mark_alive` above has already recorded.
end

local function on_status(kind, detail)
  if kind == "connected" then
    state.status = nil
  elseif kind == "waiting" then
    state.status = detail or "waiting for a session"
  elseif kind == "lost" then
    state.status = "reconnecting…"
  elseif kind == "refused" then
    state.status = "refused: " .. tostring(detail)
  elseif kind == "gave_up" then
    state.status = "lost the daemon — :Rote to retry"
  end
  redraw()
end

--- Start watching. Idempotent.
function M.start()
  if state.started then
    return
  end
  state.started = true
  local stream = require("rote.stream").new({
    on_event = on_event,
    on_status = on_status,
    cwd = vim.fn.getcwd(),
  })
  state.stream = stream
  stream:start()
end

function M.stop()
  if state.stream then
    state.stream:stop()
    state.stream = nil
  end
  state.started = false
end

--- Open the panel, or focus it if it is already up.
function M.open()
  require("rote.ui").open(state.cfg)
  M.start()
  redraw()
end

function M.toggle()
  local ui = require("rote.ui")
  if ui.is_open() then
    ui.close()
  else
    M.open()
  end
end

function M.setup(opts)
  state.cfg = config.merge(opts)
  require("rote.input").setup(state.cfg)

  local group = vim.api.nvim_create_augroup("rote", { clear = true })

  -- Remember which window is *not* the panel, so a jump never lands in it.
  vim.api.nvim_create_autocmd({ "WinEnter", "BufEnter" }, {
    group = group,
    callback = function()
      require("rote.jump").track()
    end,
  })

  -- Watch real files for how their content arrives.
  vim.api.nvim_create_autocmd("BufReadPost", {
    group = group,
    callback = function(a)
      if vim.bo[a.buf].buftype == "" then
        require("rote.input").attach(a.buf)
      end
    end,
  })

  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = group,
    callback = function()
      M.stop()
    end,
  })

  if state.cfg.auto_open then
    -- Deferred: `setup` runs during startup, and a panel that steals the layout
    -- before the first buffer is drawn is a rude way to say hello.
    vim.schedule(function()
      require("rote.endpoint").fetch({ ensure = false, cwd = vim.fn.getcwd() }, function(ep)
        if ep then
          vim.schedule(M.open)
        end
      end)
    end)
  end
end

return M
