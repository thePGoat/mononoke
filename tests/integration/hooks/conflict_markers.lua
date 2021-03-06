local allowed_suffixes = {".*%.rst$", ".*%.markdown$", ".*%.md$", ".*%.rdoc$"}

hook = function (ctx)
  for _, suffix in ipairs(allowed_suffixes) do
    if ctx.file.path:match(suffix) then
      return true
    end
  end

  local content = ctx.file.content()
  -- Consider that file is binary if it has \0
  -- And do not check binary files
  if content:find('\0') then
    return true
  end

  local error_msg = ("Conflict markers were found in file '%s'"):format(ctx.file.path)
  if content:match("^>>>>>>> ") then
    return false, error_msg
  elseif content:match("^<<<<<<< ") then
    return false, error_msg
  elseif content:match("^=======$") then
    return false, error_msg
  end
  return true
end
