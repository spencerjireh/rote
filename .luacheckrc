-- luacheck configuration for the nvim front end.
--
-- Run by `just lua-lint` and by the `lua` job in CI. Not part of `just gate`
-- and never in .rote.toml's [checks]: those run at `rote done` on other
-- people's machines, where requiring luacheck would be rude.

-- nvim embeds LuaJIT.
std = "luajit"

-- `vim` is the only name the plugin uses that LuaJIT does not provide. Every
-- other global it touches -- require, table, string, type, math, ipairs, error,
-- pcall, tostring, next, setmetatable -- is standard.
--
-- `globals` rather than `read_globals`, which was tried first and rejects the
-- way nvim is actually configured: setting an option *is* a field write, so
-- `vim.bo[buf].buftype = "nofile"`, `vim.g.loaded_rote = true` and the
-- `vim.paste` override all became "setting read-only field of global vim" --
-- 21 warnings, none of them a defect.
--
-- This gives up catching a write to `vim` itself, which nothing here does and
-- which would be obvious in review. It keeps the check that matters: a typo'd
-- global still fails, and in Lua that would otherwise read as nil and be
-- silently wrong at run time rather than loudly wrong at lint time.
globals = { "vim" }
