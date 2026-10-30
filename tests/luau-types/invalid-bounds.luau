--!strict

ui.bounds("not a widget")
local widget = ui.text { text = "anchor" }
local bounds = ui.bounds(widget)
if bounds then
    local width: string = bounds.visual.width
end

return ui.panel { children = { widget } }
