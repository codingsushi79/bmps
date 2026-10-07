-- beamhost bridge plugin for BeamMP-Server.
--
-- Installed by beamhost into Resources/Server/beamhost/ and rewritten on
-- every start, so edits here are overwritten. It does two small jobs, once a
-- second, and nothing else:
--
--   1. writes beamhost/status.json (players, vehicles) for the daemon
--   2. runs commands the daemon drops into beamhost/cmd/*.cmd
--
-- Files are used instead of a socket because they need nothing beyond the
-- stock server API, survive a daemon restart, and cost one small write per
-- second. Status is written to a temp file and renamed, so the daemon never
-- reads half a document.

local DIR = "beamhost"
local STATUS = DIR .. "/status.json"
local STATUS_TMP = DIR .. "/status.json.tmp"
local CMD_DIR = DIR .. "/cmd"
local started = os.time()

local function ensure_dirs()
  if not FS.Exists(DIR) then FS.CreateDirectory(DIR) end
  if not FS.Exists(CMD_DIR) then FS.CreateDirectory(CMD_DIR) end
end

local function json_string(s)
  s = tostring(s or "")
  s = s:gsub('[%c"\\]', function(c)
    if c == '"' then return '\\"' end
    if c == "\\" then return "\\\\" end
    if c == "\n" then return "\\n" end
    if c == "\t" then return "\\t" end
    return string.format("\\u%04x", string.byte(c))
  end)
  return '"' .. s .. '"'
end

local function count(t)
  local n = 0
  if type(t) == "table" then for _ in pairs(t) do n = n + 1 end end
  return n
end

local function write_status()
  local parts = {}
  for id, name in pairs(MP.GetPlayers() or {}) do
    local guest = false
    if MP.IsPlayerGuest then guest = MP.IsPlayerGuest(id) and true or false end
    parts[#parts + 1] = string.format(
      '{"id":%d,"name":%s,"vehicles":%d,"guest":%s}',
      id, json_string(name), count(MP.GetPlayerVehicles(id)), tostring(guest))
  end
  local body = string.format('{"time":%d,"started":%d,"players":[%s]}',
    os.time(), started, table.concat(parts, ","))
  local f = io.open(STATUS_TMP, "w")
  if not f then return end
  f:write(body)
  f:close()
  os.rename(STATUS_TMP, STATUS)
end

-- One command per file: `say <text>`, `kick <id> <reason>`, `dm <id> <text>`.
local function run(line)
  local verb, rest = line:match("^(%S+)%s*(.*)$")
  if verb == "say" then
    MP.SendChatMessage(-1, rest)
  elseif verb == "kick" then
    local id, reason = rest:match("^(%-?%d+)%s*(.*)$")
    if id then MP.DropPlayer(tonumber(id), reason ~= "" and reason or "Kicked") end
  elseif verb == "dm" then
    local id, text = rest:match("^(%-?%d+)%s*(.*)$")
    if id then MP.SendChatMessage(tonumber(id), text) end
  elseif verb then
    print("[beamhost] unknown command: " .. verb)
  end
end

local function drain_commands()
  local files = FS.ListFiles(CMD_DIR)
  if not files then return end
  table.sort(files)
  for _, name in ipairs(files) do
    if name:sub(-4) == ".cmd" then
      local path = CMD_DIR .. "/" .. name
      local f = io.open(path, "r")
      if f then
        local data = f:read("*a")
        f:close()
        FS.Remove(path)
        for line in data:gmatch("[^\r\n]+") do run(line) end
      end
    end
  end
end

function beamhostTick()
  local ok, err = pcall(function()
    drain_commands()
    write_status()
  end)
  if not ok then print("[beamhost] tick failed: " .. tostring(err)) end
end

function beamhostJoin(id)
  print("[beamhost] join " .. id .. " " .. tostring(MP.GetPlayerName(id)))
  write_status()
end

function beamhostLeave(id)
  print("[beamhost] leave " .. id .. " " .. tostring(MP.GetPlayerName(id)))
end

ensure_dirs()
MP.RegisterEvent("beamhostTick", "beamhostTick")
MP.RegisterEvent("onPlayerJoin", "beamhostJoin")
MP.RegisterEvent("onPlayerDisconnect", "beamhostLeave")
MP.CreateEventTimer("beamhostTick", 1000)
write_status()
print("[beamhost] bridge online")
