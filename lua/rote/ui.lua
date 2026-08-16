-- The panel.
--
-- Renders from the Snapshot JSON rather than reproducing `render_frame`'s ANSI
-- output, because a terminal frame inside a buffer is the worst of both.
--
-- The layout choice that matters: buffer text is context + the new lines with
-- **no diff prefixes**, because that is exactly what the region will look like
-- when you are done — a syntactically coherent chunk. Set the filetype from the
-- source path and treesitter highlights the code you are about to type, like
-- code. Everything else is decoration that does not disturb the parse.

local M = {}

local NS = vim.api.nvim_create_namespace("rote")

local state = {
  buf = nil,
  win = nil,
}

local function ensure_buf()
  if state.buf and vim.api.nvim_buf_is_valid(state.buf) then
    return state.buf
  end
  local buf = vim.api.nvim_create_buf(false, true)
  vim.bo[buf].buftype = "nofile"
  vim.bo[buf].bufhidden = "hide"
  vim.bo[buf].swapfile = false
  vim.bo[buf].buflisted = false
  vim.bo[buf].modifiable = false
  vim.b[buf].rote = true
  vim.api.nvim_buf_set_name(buf, "rote://queue")
  state.buf = buf
  M._map(buf)
  return buf
end

--- Buffer-local keys, mirroring the terminal pane exactly so muscle memory
--- transfers between the two.
-- The s/k/r/g bindings below exist three times, once per front end, because
-- they are three languages: here, in src/pane.rs, and in src/web/index.html.
-- The wire protocol underneath is version-pinned and cannot drift; these
-- bindings can, so change all three together. Same for the counts line.
function M._map(buf)
  local verbs = require("rote.verbs")
  local map = function(lhs, fn)
    vim.keymap.set("n", lhs, fn, { buffer = buf, nowait = true, silent = true })
  end
  map("s", verbs.skip)
  map("k", function()
    verbs.resolve("keep")
  end)
  map("r", function()
    verbs.resolve("retry")
  end)
  map("g", verbs.refresh)
  map("q", M.close)
  map("<CR>", function()
    require("rote.jump").force()
  end)
end

function M.is_open()
  return state.win and vim.api.nvim_win_is_valid(state.win)
end

function M.open(cfg)
  local buf = ensure_buf()
  if M.is_open() then
    return state.win
  end
  local width = math.min(cfg.width, math.floor(vim.o.columns * cfg.width_ratio))
  vim.cmd("botright " .. width .. "vsplit")
  local win = vim.api.nvim_get_current_win()
  vim.api.nvim_win_set_buf(win, buf)
  vim.wo[win].wrap = false
  vim.wo[win].number = false
  vim.wo[win].relativenumber = false
  vim.wo[win].signcolumn = "no"
  vim.wo[win].foldcolumn = "0"
  vim.wo[win].list = false
  vim.wo[win].cursorline = false
  vim.wo[win].winfixwidth = true
  state.win = win
  return win
end

function M.close()
  if M.is_open() then
    vim.api.nvim_win_close(state.win, true)
  end
  state.win = nil
end

function M.focus()
  if M.is_open() then
    vim.api.nvim_set_current_win(state.win)
  end
end

local function set_lines(buf, lines)
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
end

local function virt(buf, row, chunks_list)
  vim.api.nvim_buf_set_extmark(buf, NS, row, 0, {
    virt_lines = chunks_list,
    virt_lines_above = true,
  })
end

--- Draw one snapshot.
function M.render(snap, status)
  local buf = ensure_buf()
  vim.api.nvim_buf_clear_namespace(buf, NS, 0, -1)

  if not snap then
    set_lines(buf, { "" })
    M._winbar(("rote — %s"):format(status or "connecting…"))
    return
  end

  local active = snap.active
  if not active then
    local msg = snap.counts and snap.counts.total > 0
        and "every hunk is accounted for — `rote done` closes the session"
      or "waiting for the agent's work to land"
    set_lines(buf, { "", "  " .. msg, "" })
    M._winbar(("rote — %s"):format(snap.task or ""))
    return
  end

  local hunk = active.hunk
  local before = hunk.context_before or {}
  local new = hunk.new_lines or {}
  local after = hunk.context_after or {}

  local lines = {}
  vim.list_extend(lines, before)
  vim.list_extend(lines, new)
  vim.list_extend(lines, after)
  if #lines == 0 then
    -- An untypeable hunk: presented whole and gated on a byte comparison.
    lines = { "  " .. (hunk.note or "verified by byte comparison, not typed") }
  end
  set_lines(buf, lines)

  -- Filetype from the *source* path, so the code is highlighted as code.
  local ft = vim.filetype.match({ filename = hunk.file }) or ""
  if vim.bo[buf].filetype ~= ft then
    vim.bo[buf].filetype = ft
  end

  local first_new = #before
  for i = 0, #new - 1 do
    vim.api.nvim_buf_set_extmark(buf, NS, first_new + i, 0, {
      line_hl_group = "DiffAdd",
    })
  end

  -- What is being replaced, as ghost text above rather than buffer text: it is
  -- not something to type, and putting it in the buffer would break the parse.
  local old = hunk.old_lines or {}
  if #old > 0 and #lines > first_new then
    local ghost = {}
    for _, l in ipairs(old) do
      table.insert(ghost, { { "- " .. l, "DiffDelete" } })
    end
    virt(buf, first_new, ghost)
  end

  if hunk.curator_note and hunk.curator_note ~= "" then
    virt(buf, 0, { { { hunk.curator_note, "Comment" } } })
  end

  if hunk.pending_divergence then
    local d = hunk.pending_divergence
    local block = { { { "", "Normal" } }, { { "you typed:", "WarningMsg" } } }
    for _, l in ipairs(d.actual or {}) do
      table.insert(block, { { "  " .. l, "DiffDelete" } })
    end
    table.insert(block, { { "it proposed:", "WarningMsg" } })
    for _, l in ipairs(d.proposed or {}) do
      table.insert(block, { { "  " .. l, "DiffAdd" } })
    end
    table.insert(block, { { "k keep yours · r keep typing", "Comment" } })
    virt(buf, math.max(#lines - 1, 0), block)
  end

  local c = snap.counts or {}
  M._winbar(
    ("rote %d/%d · %s:%d · %d pending %d typed%s"):format(
      active.position or 0,
      active.total or 0,
      hunk.file,
      active.anchor_line or 0,
      c.pending or 0,
      c.typed or 0,
      status and (" · " .. status) or ""
    )
  )
end

function M._winbar(text)
  if M.is_open() then
    vim.wo[state.win].winbar = text:gsub("%%", "%%%%")
  end
end

function M.buf()
  return state.buf
end

return M
