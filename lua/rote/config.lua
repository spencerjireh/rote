local M = {}

M.defaults = {
  --- Panel width, capped at a share of the columns available.
  width = 80,
  width_ratio = 0.4,

  --- "on_change" moves the cursor when the active hunk changes; "never" leaves
  --- it alone and you drive with :RoteJump. Either way it is never moved while
  --- you are in insert mode or already inside the region.
  auto_jump = "on_change",

  --- "precise" reports a paste only when nvim actually routed one — bracketed
  --- paste, OSC 52, nvim_paste, or a put key. No false positives.
  ---
  --- "heuristic" additionally treats any single change larger than
  --- `paste_threshold` bytes, or spanning more than one line, as a paste. It
  --- catches more and may misfire on undo, which is why it is not the default:
  --- a wrong `pasted` is worse than an honest `unknown`.
  paste_detection = "precise",
  paste_threshold = 24,

  --- Open the panel automatically when a session is found.
  auto_open = true,
}

function M.merge(opts)
  return vim.tbl_deep_extend("force", vim.deepcopy(M.defaults), opts or {})
end

return M
