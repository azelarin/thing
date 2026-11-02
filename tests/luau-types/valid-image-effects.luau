--!strict

local paused = ui.state(false)
local focus = ui.motion(0, { kind = "transition", duration = 200, easing = "out_cubic" })

return ui.panel {
    children = {
        -- a soft, muted backdrop from the current artwork.
        ui.image { source = "cover.png", fit = "cover", blur = 24, saturation = 0.6 },
        -- gray out paused artwork and bring its color back smoothly.
        ui.image {
            source = "cover.png",
            saturation = ui.transition(function()
                return if paused:get() then 0 else 1
            end, { duration = 150 }),
            blur = ui.spring(function()
                return if paused:get() then 6 else 0
            end, { stiffness = 200, damping = 1 }),
        },
        ui.image { icon = "folder", blur = function() return focus:get() * 4 end },
        ui.button {
            on_click = function()
                paused:set(not paused:get())
                focus:to(1)
            end,
        },
    },
}
