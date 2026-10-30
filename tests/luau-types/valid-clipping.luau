--!strict

local rounded = ui.state(true)
local function clip(): ThingClipMode
    return if rounded:get() then "rounded" else "bounds"
end

return ui.panel {
    children = {
        ui.stack {
            style = { clip = clip, radius = 24 },
            children = {
                ui.text { text = "clipped", style = { clip = "rounded", radius = 4 } },
                ui.text_input { value = "editable", on_change = function() end, style = { clip = "bounds" } },
            },
        },
    },
}
