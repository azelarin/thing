--!strict

services.system_tray.open_menu(42)
services.system_tray.close_menu(false)
services.system_tray.activate_menu_item({})

local item = services.system_tray.items:get()[1]
if item.menu ~= nil then
    local _wrong_label: number = item.menu.children[1].label
    local _wrong_loading: number = item.menu.loading
end

return ui.panel {}
