--!strict

local label = ui.text { text = "anchor" }
local center = ui.state(0)

return ui.panel {
    children = {
        ui.button {
            children = { label },
            on_click = function()
                local bounds = ui.bounds(label)
                if bounds then
                    local surface: string = bounds.surface
                    local width: number = bounds.layout.width
                    center:set(bounds.visual.x + bounds.visual.width / 2)
                    print(surface, width)
                end
            end,
        },
        ui.text { text = function() return tostring(center:get()) end },
    },
}
