--!strict

local record = services.notifications.history:get()[1]
local _at: number = record.closed_at_ms
local _reason: "dismissed" = record.closed_reason
services.notifications.reply(record.key)
services.notifications.clear_history(record.key)

return ui.panel {}
