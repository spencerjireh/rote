-- Finding the daemon.
--
-- By shelling out to `rote endpoint --json` rather than reading daemon.json
-- ourselves: the file lives under a SHA-256 of the canonicalized repo root, and
-- reimplementing that hashing in Lua is a thing to get subtly wrong once and
-- then never notice.
--
-- Re-read on every reconnect, never cached across one, because a restarted
-- daemon has a new port *and* a new token.

local M = {}

--- @class rote.Endpoint
--- @field url string
--- @field port integer
--- @field token string
--- @field project_hash string
--- @field wire_version integer

--- Ask `rote` where the daemon is.
--- @param opts { ensure: boolean, cwd: string|nil }
--- @param cb fun(ep: rote.Endpoint|nil, err: string|nil)
function M.fetch(opts, cb)
  local cmd = { "rote", "endpoint", "--json" }
  if opts.ensure then
    table.insert(cmd, "--ensure")
  end

  vim.system(cmd, { cwd = opts.cwd, text = true }, function(res)
    -- `--json` prints an object on failure too, so stdout is parsed
    -- unconditionally rather than switching on the exit code first.
    local ok, body = pcall(vim.json.decode, res.stdout or "", {
      luanil = { object = true, array = true },
    })
    if not ok or type(body) ~= "table" then
      local why = (res.stderr or ""):gsub("%s+$", "")
      if why == "" then
        why = "`rote endpoint` did not answer (is rote on your PATH?)"
      end
      return cb(nil, why)
    end
    if not body.ok then
      return cb(nil, M.explain(body.error, body.pid))
    end
    cb(body, nil)
  end)
end

--- Turn an error code into something worth reading.
function M.explain(code, pid)
  if code == "no_session" then
    return "no rote session here. `rote start \"what you're working on\"`"
  elseif code == "opaque_owner" then
    return ("something owns this queue but does not serve HTTP%s — a `rote watch --local` pane does that")
      :format(pid and (" (pid " .. pid .. ")") or "")
  elseif code == "no_daemon" then
    return "no rote daemon running"
  end
  return "rote endpoint: " .. tostring(code or "unknown error")
end

return M
