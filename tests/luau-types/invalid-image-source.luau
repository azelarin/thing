--!strict

local item = services.system_tray.items:get()[1]
local wrong_icon = ui.image { icon = item.icon }
local wrong_app_id = ui.image { app_id = item.icon }

return ui.panel { children = { wrong_icon, wrong_app_id } }
