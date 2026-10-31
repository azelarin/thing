--!strict

local n = services.notifications
local tick = ui.state(0)
ui.timer { interval = 100, on_timeout = function() tick:set(tick:get() + 1) end }
return ui.panel { children = function()
    local rows = {}
    for _, record in n.active:get() do
        local paused: boolean = record.expiry_paused
        table.insert(rows, ui.button {
            key = record.key,
            on_enter = function() n.set_expiry_paused(record.key, true) end,
            on_leave = function() n.set_expiry_paused(record.key, false) end,
            children = { ui.text { text = function()
                tick:get()
                local remaining: number? = n.remaining(record.key)
                return if remaining then tostring(remaining) else 'no countdown'
            end } },
        })
    end
    return rows
end }
