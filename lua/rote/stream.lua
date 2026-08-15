-- The event stream, over a raw socket.
--
-- `vim.uv` rather than `jobstart({"curl", "-N", ...})`, on rote's own
-- precedent: it hand-rolled an HTTP client in Rust rather than take a
-- dependency for three request shapes, and the read side here is one request
-- line, skip to the blank line, then split on "\n\n". That is ninety lines and
-- no assumption about what is on PATH.
--
-- Nothing here writes. Every mutation shells out to `rote` (see verbs.lua),
-- which already routes correctly through the engine-token check — so the plugin
-- never has to hold a token, construct a JSON body, or reason about the
-- staleness generation.

local endpoint = require("rote.endpoint")

local uv = vim.uv or vim.loop

local M = {}

--- Reconnect ladder, matching the terminal pane's.
local BACKOFF_MIN = 250
local BACKOFF_MAX = 2000
local GIVE_UP_MS = 30000

--- The daemon writes a bare ":" comment every 15s of silence. Three of those
--- missed means the socket is open but nothing is behind it any more, which a
--- read error would never surface on its own.
local SILENCE_MS = 45000
local WATCHDOG_MS = 5000

--- @class rote.Stream
--- @field on_event fun(ev: table)
--- @field on_status fun(state: string, detail: string|nil)
local Stream = {}
Stream.__index = Stream

--- @param handlers { on_event: fun(ev: table), on_status: fun(state: string, detail: string|nil) }
function M.new(handlers)
  return setmetatable({
    on_event = handlers.on_event,
    on_status = handlers.on_status,
    cwd = handlers.cwd,
    handle = nil,
    buffer = "",
    head_done = false,
    backoff = BACKOFF_MIN,
    lost_since = nil,
    last_rx = nil,
    stopped = false,
    timer = nil,
  }, Stream)
end

function Stream:start()
  self.stopped = false
  self:_watchdog()
  self:_connect()
end

function Stream:stop()
  self.stopped = true
  self:_drop()
  if self.timer then
    self.timer:stop()
    self.timer:close()
    self.timer = nil
  end
end

function Stream:_drop()
  if self.handle and not self.handle:is_closing() then
    self.handle:read_stop()
    self.handle:close()
  end
  self.handle = nil
  self.buffer = ""
  self.head_done = false
end

--- Every attempt re-asks where the daemon is. A restarted one has a new port
--- and a new token, so a cached endpoint would reconnect to nothing forever.
function Stream:_connect()
  if self.stopped then
    return
  end
  endpoint.fetch({ ensure = true, cwd = self.cwd }, function(ep, err)
    if self.stopped then
      return
    end
    if not ep then
      return vim.schedule(function()
        self.on_status("waiting", err)
        self:_retry()
      end)
    end
    vim.schedule(function()
      self:_open(ep)
    end)
  end)
end

function Stream:_open(ep)
  if self.stopped then
    return
  end
  local sock = uv.new_tcp()
  self.handle = sock
  self.endpoint = ep

  sock:connect("127.0.0.1", ep.port, function(cerr)
    if cerr then
      return vim.schedule(function()
        self.on_status("waiting", "connect: " .. cerr)
        self:_retry()
      end)
    end
    -- The query token, because that is the path a browser has to use and
    -- there is no reason for two spellings.
    local req = table.concat({
      "GET /events?token=" .. ep.token .. " HTTP/1.1\r\n",
      "Host: 127.0.0.1:" .. ep.port .. "\r\n",
      "Connection: close\r\n\r\n",
    })
    sock:write(req)
    self.last_rx = uv.now()
    sock:read_start(function(rerr, chunk)
      if rerr or not chunk then
        return vim.schedule(function()
          self:_drop()
          self.on_status("lost", rerr)
          self:_retry()
        end)
      end
      -- Updated on *every* chunk, keepalive comments included: they are the
      -- only proof a silent stream is still attached to something.
      self.last_rx = uv.now()
      self:_feed(chunk)
    end)
  end)
end

--- Accumulate, skip the response head once, then dispatch whole frames.
function Stream:_feed(chunk)
  self.buffer = self.buffer .. chunk

  if not self.head_done then
    local blank = self.buffer:find("\r\n\r\n", 1, true)
    if not blank then
      return
    end
    local head = self.buffer:sub(1, blank - 1)
    self.buffer = self.buffer:sub(blank + 4)
    if not head:match("^HTTP/1%.1 200") then
      return vim.schedule(function()
        self:_drop()
        self.on_status("refused", head:match("^[^\r\n]*"))
        self:_retry()
      end)
    end
    self.head_done = true
    vim.schedule(function()
      self.on_status("connected", nil)
    end)
  end

  while true do
    local sep = self.buffer:find("\n\n", 1, true)
    if not sep then
      return
    end
    local frame = self.buffer:sub(1, sep - 1)
    self.buffer = self.buffer:sub(sep + 2)
    self:_dispatch(frame)
  end
end

function Stream:_dispatch(frame)
  local data = {}
  for line in (frame .. "\n"):gmatch("([^\n]*)\n") do
    -- A line starting with ":" is a comment. That is the keepalive, and
    -- dispatching an empty frame for it would redraw the panel every 15s.
    local payload = line:match("^data: ?(.*)$")
    if payload then
      table.insert(data, payload)
    end
  end
  if #data == 0 then
    return
  end

  local text = table.concat(data, "\n")
  -- `luanil` matters more than it looks: without it a JSON null decodes to
  -- `vim.NIL`, which is *truthy* in Lua, so `if snap.active then` fires on an
  -- empty queue and the panel renders nothing-shaped garbage.
  local ok, ev = pcall(vim.json.decode, text, {
    luanil = { object = true, array = true },
  })
  if not ok or type(ev) ~= "table" then
    return
  end

  -- Dispatch on the JSON type tag rather than the SSE event name, so a daemon
  -- that grows a new event variant does not break an old plugin.
  vim.schedule(function()
    self.on_event(ev)
  end)
end

function Stream:_retry()
  if self.stopped then
    return
  end
  self.lost_since = self.lost_since or uv.now()
  if uv.now() - self.lost_since >= GIVE_UP_MS then
    self.on_status("gave_up", nil)
    self.stopped = true
    return
  end
  local wait = self.backoff
  self.backoff = math.min(self.backoff * 2, BACKOFF_MAX)
  vim.defer_fn(function()
    self:_connect()
  end, wait)
end

--- Reset the ladder once something actually arrives.
function Stream:mark_alive()
  self.lost_since = nil
  self.backoff = BACKOFF_MIN
end

--- Catches the case a read error never will: a socket still open with a daemon
--- behind it that has stopped saying anything at all.
function Stream:_watchdog()
  self.timer = uv.new_timer()
  self.timer:start(WATCHDOG_MS, WATCHDOG_MS, function()
    if self.stopped or not self.last_rx then
      return
    end
    if uv.now() - self.last_rx > SILENCE_MS then
      self.last_rx = uv.now()
      vim.schedule(function()
        self:_drop()
        self.on_status("lost", "no keepalive")
        self:_retry()
      end)
    end
  end)
end

return M
