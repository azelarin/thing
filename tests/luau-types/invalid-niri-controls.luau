--!strict

services.niri.set_overview("true")
services.niri.switch_layout(false)
local layouts = services.niri.keyboard_layouts:get()
if layouts then
    local name: number = layouts.names[1]
end
return ui.panel {}
