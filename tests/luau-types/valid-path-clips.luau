--!strict

local holed = ui.state(true)

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                holed:set(not holed:get())
            end,
            style = {
                width = 64,
                height = 64,
                clip = function(): ThingClipMode
                    return {
                        path = "M32 0 A32 32 0 1 1 32 64 A32 32 0 1 1 32 0 Z M24 24 H40 V40 H24 Z",
                        view_box = { x = 0, y = 0, width = 64, height = 64 },
                        fit = "cover",
                        fill_rule = if holed:get() then "evenodd" else "nonzero",
                    }
                end,
            },
        },
        ui.stack { style = { clip = "rounded", radius = 8 } },
    },
}
