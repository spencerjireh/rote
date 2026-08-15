-- Command declarations only. Nothing here requires `rote.*`, so a user who
-- never opens the panel pays nothing for having the plugin installed.

if vim.g.loaded_rote then
  return
end
vim.g.loaded_rote = true

if vim.fn.has("nvim-0.10") ~= 1 then
  vim.notify("rote.nvim needs nvim 0.10 or newer", vim.log.levels.WARN)
  return
end

local function cmd(name, fn, opts)
  vim.api.nvim_create_user_command(name, fn, opts or {})
end

cmd("Rote", function()
  require("rote").toggle()
end, { desc = "rote: open or close the panel" })

cmd("RoteClose", function()
  -- The window only. The stream keeps running, so the queue keeps advancing
  -- and the anchor sign stays live in your buffers.
  require("rote.ui").close()
end, { desc = "rote: close the panel, keep watching" })

cmd("RoteDetach", function()
  require("rote").stop()
  require("rote.ui").close()
end, { desc = "rote: stop watching" })

cmd("RoteSkip", function(a)
  require("rote.verbs").skip(a.args ~= "" and a.args or nil)
end, { nargs = "?", desc = "rote: leave a hunk untyped" })

cmd("RoteResolve", function(a)
  local choice = a.fargs[1]
  if choice ~= "keep" and choice ~= "retry" then
    return vim.notify("rote: :RoteResolve keep|retry", vim.log.levels.ERROR)
  end
  require("rote.verbs").resolve(choice, a.fargs[2])
end, {
  nargs = "+",
  desc = "rote: answer a divergence question",
  complete = function()
    return { "keep", "retry" }
  end,
})

cmd("RoteReport", function(a)
  local input = a.fargs[1]
  if input ~= "typed" and input ~= "pasted" then
    return vim.notify("rote: :RoteReport typed|pasted", vim.log.levels.ERROR)
  end
  require("rote.verbs").report(input, a.fargs[2])
end, {
  nargs = "+",
  desc = "rote: say how a hunk arrived",
  complete = function()
    return { "typed", "pasted" }
  end,
})

cmd("RoteRefresh", function()
  require("rote.verbs").refresh()
end, { desc = "rote: force a re-diff of the trees" })

cmd("RoteJump", function()
  require("rote.jump").force()
end, { desc = "rote: go to the active hunk" })

cmd("RoteStatus", function()
  local snap = require("rote").snapshot()
  if not snap then
    return vim.notify("rote: not attached", vim.log.levels.INFO)
  end
  local c = snap.counts or {}
  vim.notify(
    ("rote: %s — %d pending, %d typed, %d skipped, %d diverged (generation %s)"):format(
      snap.task or "",
      c.pending or 0,
      c.typed or 0,
      c.skipped or 0,
      c.diverged or 0,
      snap.generation
    )
  )
end, { desc = "rote: what the plugin is looking at" })
