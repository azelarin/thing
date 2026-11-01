--!strict

local settings = ui.window_state("settings")
local panel_fields: ThingWindowProperties = {}
panel_fields.layer = "top"
ui.window(panel_fields)

return {
    ui.window {
        key = "settings",
        size = { width = 480 },
        title = 3,
        on_close = function(reason: string) end,
        children = {
            ui.button {
                on_click = function()
                    settings.set_maximized("yes")
                    settings.width:set(1)
                end,
            },
        },
    },
}
