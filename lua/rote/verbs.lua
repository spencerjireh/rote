-- Mutations, by shelling out.
--
-- `rote skip` / `resolve` / `report` already decide for themselves whether to
-- mutate directly or send a verb to the daemon (the engine-token check in
-- `daemon::owner`). Reusing that means the plugin never holds a bearer token,
-- never builds a JSON body, and never has to reason about the staleness
-- generation — three things it would otherwise get to be wrong about
-- independently of the pane.
--
-- One process spawn per keypress. At human rates that is free, and it buys a
-- plugin that cannot drift from the CLI's semantics.

local M = {}

local function run(args, on_done)
  vim.system(vim.list_extend({ "rote" }, args), { text = true }, function(res)
    vim.schedule(function()
      if res.code ~= 0 then
        local why = (res.stderr or ""):gsub("%s+$", "")
        vim.notify("rote: " .. (why ~= "" and why or "command failed"), vim.log.levels.WARN)
      elseif on_done then
        on_done(res)
      end
    end)
  end)
end

local function active_id()
  local snap = require("rote").snapshot()
  return snap and snap.active and snap.active.hunk.id or nil
end

function M.skip(id)
  id = id or active_id()
  if not id then
    return vim.notify("rote: nothing to skip", vim.log.levels.INFO)
  end
  run({ "skip", id })
end

function M.resolve(choice, id)
  id = id or active_id()
  if not id then
    return vim.notify("rote: nothing to resolve", vim.log.levels.INFO)
  end
  run({ "resolve", id, choice })
end

function M.report(input, id)
  id = id or active_id()
  if not id then
    return
  end
  run({ "report", id, input })
end

--- Force a re-diff of the trees.
---
--- There is no `rote refresh`; `next` recomputes on its way to printing, which
--- is the same thing happening for a different stated reason. The output is
--- discarded — the daemon publishes a snapshot either way, and that is what the
--- panel draws from.
function M.refresh()
  run({ "next", "--json" })
end

return M
