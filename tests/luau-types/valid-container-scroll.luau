--!strict

local month = ui.state(0)
local workspace = ui.state(1)

local function step(y: number): number
    return if y > 0 then 1 elseif y < 0 then -1 else 0
end

return ui.panel {
    on_scroll = function(_x, y, event)
        if event.ctrl then
            workspace:set(workspace:get() + step(y))
            return true
        end
        return false
    end,
    children = {
        ui.row {
            on_scroll = function(x, _y, event)
                -- horizontal wheels and shift-scrolling change the month.
                local delta = if event.shift then _y else x
                if delta == 0 then
                    return false
                end
                month:set(month:get() + step(delta))
                return nil
            end,
            children = {
                ui.text { text = function() return `month {month:get()}` end },
            },
        },
        ui.scroll {
            on_scroll = function(_x, _y, event)
                return event.source == 'wheel_tilt'
            end,
            children = { ui.column { children = {} } },
        },
        ui.button {
            children = {
                ui.popup {
                    on_scroll = function()
                        return true
                    end,
                },
            },
        },
    },
}
