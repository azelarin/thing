--!strict

local depth = ui.motion(0, { kind = "spring", stiffness = 170, damping = 0.8 })
local fade = ui.motion(0, { kind = "transition", duration = 180, easing = "out_cubic" })

return ui.panel {
    children = {
        ui.button {
            on_secondary_click = function()
                depth:stop()
                fade:jump(0)
            end,
            on_click = function()
                depth:to(100)
                fade:to(1)
            end,
        },
        ui.path {
            view_box = { x = 0, y = 0, width = 100, height = 100 },
            data = function()
                return string.format("M0 0 L100 0 L100 %f Z", depth:get())
            end,
            style = {
                opacity = function() return fade:get() end,
                translate_x = function() return math.min(20, depth:velocity() * 0.01) end,
            },
        },
    },
}
