--!strict

local _missing: ThingClipPath = { path = "M0 0 H1 V1 Z" }
local _rule: ThingClipPath = {
    path = "M0 0 H1 V1 Z",
    view_box = { x = 0, y = 0, width = 1, height = 1 },
    fill_rule = "holes",
}

return ui.panel { style = { clip = "circle" } }
