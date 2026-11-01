--!strict

ui.window { cursor = "crosshair", children = {} }

local dragging = ui.state(false)

return ui.panel {
    cursor = "default",
    children = {
        ui.button {
            cursor = function(): ThingCursor?
                return if dragging:get() then "grabbing" else nil
            end,
            on_click = function()
                dragging:set(not dragging:get())
            end,
        },
        ui.text_input { value = "", on_change = function() end, cursor = "text" },
        ui.row { cursor = "col_resize", style = { width = 4 } },
        ui.pointer_area { cursor = "crosshair", on_pointer = function() return false end },
    },
}
