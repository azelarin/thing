--!strict

local grouped = ui.state(true)

return ui.panel {
    style = { composite = true },
    children = {
        ui.stack {
            style = {
                composite = function() return grouped:get() end,
                opacity = ui.transition(function()
                    return if grouped:get() then 0.5 else 1
                end, { duration = 200 }),
            },
        },
    },
}
