-- Following the queue with the cursor, without stealing it.
--
-- This is the curator's pin problem restated: a snapshot arrives on every save,
-- and moving someone's cursor while they are typing is the same sin as
-- reordering the hunk they are working on. Five guards, all load-bearing.

local M = {}

local state = {
  last_id = nil,
  code_win = nil,
  pending = nil,
}

local NS = vim.api.nvim_create_namespace("rote-anchor")

--- Remember the last window that was not the panel, so a jump never lands in
--- the panel itself.
function M.track()
  local win = vim.api.nvim_get_current_win()
  local buf = vim.api.nvim_win_get_buf(win)
  if not vim.b[buf].rote then
    state.code_win = win
  end
end

local function target_window()
  if state.code_win and vim.api.nvim_win_is_valid(state.code_win) then
    return state.code_win
  end
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    if not vim.b[vim.api.nvim_win_get_buf(w)].rote then
      return w
    end
  end
  return nil
end

--- Is the cursor already somewhere inside the region being typed?
---
--- The anchor is where the region *starts*. Jumping there while someone is on
--- line five of it throws them back to the top of what they are writing, which
--- is exactly the interruption this whole design avoids elsewhere.
local function already_inside(win, path, line, height)
  local buf = vim.api.nvim_win_get_buf(win)
  local name = vim.api.nvim_buf_get_name(buf)
  if name == "" or vim.fn.fnamemodify(name, ":p") ~= vim.fn.fnamemodify(path, ":p") then
    return false
  end
  local row = vim.api.nvim_win_get_cursor(win)[1]
  return row >= line and row <= line + height + 3
end

--- Move to the active hunk, if every guard agrees.
--- @param presented table|nil
--- @param cfg table
function M.follow(presented, cfg)
  if not presented then
    state.last_id = nil
    return
  end
  M.sign(presented)

  if cfg.auto_jump == "never" then
    return
  end
  -- A snapshot arrives on every save. Only a *different hunk* is news.
  if presented.hunk.id == state.last_id then
    return
  end
  state.last_id = presented.hunk.id
  M._go(presented, false)
end

--- `:RoteJump` — the same move, minus the "only on change" guard.
function M.force()
  local snap = require("rote").snapshot()
  if snap and snap.active then
    M._go(snap.active, true)
  end
end

function M._go(presented, forced)
  local mode = vim.api.nvim_get_mode().mode
  if not forced and (mode:find("^i") or mode:find("^R") or mode == "t") then
    -- Mid-insert. Hold it and go when they come up for air.
    state.pending = presented
    vim.api.nvim_create_autocmd("InsertLeave", {
      once = true,
      callback = function()
        local p = state.pending
        state.pending = nil
        if p then
          M._go(p, false)
        end
      end,
    })
    return
  end

  local win = target_window()
  if not win then
    return
  end
  local path = presented.real_path
  local line = presented.anchor_line or 1
  local height = #(presented.hunk.new_lines or {})

  if not forced and already_inside(win, path, line, height) then
    return
  end

  vim.api.nvim_win_call(win, function()
    if vim.fn.fnamemodify(vim.api.nvim_buf_get_name(0), ":p") ~= vim.fn.fnamemodify(path, ":p") then
      vim.cmd.edit(vim.fn.fnameescape(path))
    end
    -- So `''` takes them back to wherever they were.
    vim.cmd("normal! m'")
    local last = vim.api.nvim_buf_line_count(0)
    vim.api.nvim_win_set_cursor(0, { math.min(line, last), 0 })
    vim.cmd("normal! zz")
  end)
end

--- Mark the anchor in the real buffer, so it is findable without the panel.
function M.sign(presented)
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) then
      vim.api.nvim_buf_clear_namespace(buf, NS, 0, -1)
    end
  end
  if not presented then
    return
  end
  local path = vim.fn.fnamemodify(presented.real_path, ":p")
  for _, buf in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(buf) and vim.fn.fnamemodify(vim.api.nvim_buf_get_name(buf), ":p") == path then
      local line = math.min(presented.anchor_line or 1, vim.api.nvim_buf_line_count(buf))
      pcall(vim.api.nvim_buf_set_extmark, buf, NS, math.max(line - 1, 0), 0, {
        sign_text = "▸",
        sign_hl_group = "DiffAdd",
      })
    end
  end
end

return M
