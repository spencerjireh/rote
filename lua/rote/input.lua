-- Telling rote how the content arrived.
--
-- This is what the plugin is actually for, beyond being a nicer pane. The
-- engine can prove *typed* only when it happens to see a partial save, so a
-- `:w`-at-the-end user gets `unknown` for work they typed every character of.
-- nvim knows the difference and nothing else does.
--
-- Both verdicts are reported, not just pastes. Reporting only pastes would
-- leave `rote done` saying "3 of 12 observed" to someone who typed all twelve,
-- which is the same accusation the silence rule exists to avoid.

local verbs = require("rote.verbs")

local M = {}

local cfg = {}
local reported = {}
local pasting = false

--- A change counts only if it lands in the hunk currently being transcribed.
---
--- That is the only hunk whose id the plugin holds reliably — `QueueItem`
--- carries `anchor_hint`, which is where a hunk was at the last recompute and
--- not where it is now. Attributing a paste to the wrong hunk is worse than
--- attributing it to none, so anything elsewhere is left `unknown`.
local function active_region()
  local snap = require("rote").snapshot()
  local p = snap and snap.active
  if not p then
    return nil
  end
  return {
    id = p.hunk.id,
    path = vim.fn.fnamemodify(p.real_path, ":p"),
    first = p.anchor_line or 1,
    last = (p.anchor_line or 1) + #(p.hunk.new_lines or {}) + 1,
  }
end

local function in_region(buf, row)
  local region = active_region()
  if not region then
    return nil
  end
  if vim.fn.fnamemodify(vim.api.nvim_buf_get_name(buf), ":p") ~= region.path then
    return nil
  end
  if row + 1 < region.first - 1 or row + 1 > region.last + 1 then
    return nil
  end
  return region.id
end

local function say(id, verdict)
  if not id or reported[id] == verdict then
    return
  end
  reported[id] = verdict
  verbs.report(verdict, id)
end

--- Watch one buffer's byte-level changes.
function M.attach(buf)
  if vim.b[buf].rote_attached or vim.b[buf].rote then
    return
  end
  vim.b[buf].rote_attached = true

  vim.api.nvim_buf_attach(buf, false, {
    on_bytes = function(_, b, _, row, _, _, _, _, _, new_rows, _, new_bytes)
      if not vim.api.nvim_buf_is_valid(b) then
        return true
      end
      local id = in_region(b, row)
      if not id then
        return
      end
      if pasting then
        vim.schedule(function()
          say(id, "pasted")
        end)
        return
      end
      -- Incremental insertion inside the region: someone is typing it.
      if new_bytes > 0 and new_bytes <= cfg.paste_threshold and new_rows <= 1 then
        vim.schedule(function()
          say(id, "typed")
        end)
      elseif cfg.paste_detection == "heuristic" and (new_bytes > cfg.paste_threshold or new_rows > 1) then
        vim.schedule(function()
          say(id, "pasted")
        end)
      end
    end,
  })
end

--- Flag the window in which a paste is landing.
---
--- Wrapping `vim.paste` catches bracketed paste, OSC 52 and `nvim_paste`, which
--- is every route the terminal takes. The put keys are the other half: `"+p` is
--- a paste by any honest definition even though nvim never calls `vim.paste`
--- for it.
function M.setup(opts)
  cfg = opts
  reported = {}

  local original = vim.paste
  vim.paste = function(lines, phase)
    pasting = true
    local ok, res = pcall(original, lines, phase)
    if phase == -1 or phase == 3 then
      vim.defer_fn(function()
        pasting = false
      end, 50)
    end
    if not ok then
      pasting = false
      error(res)
    end
    return res
  end

  for _, lhs in ipairs({ "p", "P", "gp", "gP" }) do
    vim.keymap.set({ "n", "x" }, lhs, function()
      pasting = true
      vim.defer_fn(function()
        pasting = false
      end, 50)
      return lhs
    end, { expr = true, desc = "rote: note a paste" })
  end
  vim.keymap.set("i", "<C-r>", function()
    pasting = true
    vim.defer_fn(function()
      pasting = false
    end, 200)
    return "<C-r>"
  end, { expr = true, desc = "rote: note a paste" })
end

--- A hunk that left and came back is a new question.
function M.forget(id)
  reported[id] = nil
end

return M
