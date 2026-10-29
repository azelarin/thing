--!strict

local media = services.media
local elapsed = ui.state(0)

ui.timer {
    interval = 500,
    repeating = true,
    autostart = true,
    on_timeout = function()
        elapsed:set(media.position() or 0)
    end,
}

return ui.panel {
    children = {
        ui.button {
            enabled = function() return media.can_seek:get() end,
            on_click = function() media.seek(-10000) end,
        },
        ui.button {
            on_click = function()
                local first = media.players:get()[1]
                if first and first.can_seek and first.length_ms then
                    media.set_position(first.length_ms / 2, first.id)
                end
            end,
        },
        ui.text {
            text = function()
                local length = media.length_ms:get()
                local rate = media.rate:get()
                return string.format('%d / %d at %gx', elapsed:get(), length or 0, rate)
            end,
        },
    },
}
