return function(reloaded)
    local done = if reloaded then '#e0a451' else '#47d16c'
    local fade = ui.motion(1, { kind = 'transition', duration = 300 })
    local spring = ui.motion(1, { kind = 'spring', stiffness = 100, damping = 0.2, epsilon = 0.02 })
    local clock = ui.motion(0, { kind = 'transition', duration = 4500 })
    local opened, started, overshot = ui.state(true), ui.state(false), ui.state(false)
    local phase = 0
    ui.timer { interval = 1, autostart = true, on_timeout = function()
        fade:to(0); spring:to(0); clock:to(1)
        opened:set(false); started:set(true); phase = 1
    end }
    ui.timer { interval = 450, autostart = true, on_timeout = function()
        opened:set(true); spring:to(1); phase = 2
    end }
    ui.timer { interval = 700, autostart = true, on_timeout = function()
        opened:set(false); spring:to(0); phase = 3
    end }
    ui.timer { interval = 20, repeating = true, autostart = true, on_timeout = function(timer)
        if spring:get() < 0 and spring:running() then overshot:set(true) end
        if phase == 3 and not spring:running() and spring:get() == 0 then timer:stop() end
    end }
    local function outgoing(motion, reopen)
        return ui.row { style = { width = 160, height = 40, gap = 8 }, children = function()
            local marker = ui.row { key = 'marker', style = { width = 16, height = 40, background = done } }
            if not reopen() and not motion:running() and motion:get() == 0 then return { marker } end
            return {
                ui.row { key = 'content', style = { width = 80, height = 40, background = '#7997ff',
                    opacity = function() return math.clamp(motion:get(), 0, 1) end } },
                marker,
            }
        end }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 220, padding = 20, background = '#0d1117' },
        children = { ui.column {
            style = { width = 600, height = 180, gap = 40, align = 'start' },
            children = {
                ui.row { style = { width = 600, height = 40, gap = 20 }, children = {
                    outgoing(fade, function() return false end),
                    ui.row { style = { width = 80, height = 40, background = function()
                        return if started:get() and not clock:running() then done else '#e84bb5'
                    end } },
                } },
                ui.row { style = { width = 600, height = 40, gap = 20 }, children = {
                    outgoing(spring, function() return opened:get() end),
                    ui.row { style = { width = 80, height = 40, background = function()
                        return if overshot:get() then done else '#e84bb5'
                    end } },
                } },
            },
        } },
    }
end
