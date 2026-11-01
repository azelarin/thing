--!strict

local widget = ui.text { text = "anchor" }
ui.geometry("not a widget")
local geometry = ui.geometry(widget)
local layout = geometry:layout()
if layout then
    local width: string = layout.width
end
ui.convert(widget, "0", 0, widget)

return ui.panel { children = { widget } }
