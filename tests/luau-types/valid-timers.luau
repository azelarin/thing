--!strict

local visible = ui.state(false)
local hide = ui.timer {
    interval = 250,
    on_timeout = function() visible:set(false) end,
}

ui.timer {
    interval = 1000,
    repeating = true,
    autostart = true,
    on_timeout = function(timer) timer:stop() end,
}

return ui.panel {
    children = { ui.button {
        on_click = function() hide:start() end,
        on_hover = function(inside)
            if inside then hide:stop() else hide:restart() end
        end,
    } },
}
