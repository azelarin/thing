return function(reloaded)
    local function sample(label, options, target, replacement, reject)
        local motion = ui.motion(0, options)
        ui.timer { interval = if reject then 2000 else 1, autostart = true,
            on_timeout = function() motion:to(target) end }
        if replacement then
            ui.timer { interval = if reject then 2800 else 150, autostart = true,
                on_timeout = function() motion:to(replacement) end }
        end
        return ui.row {
            style = { width = 600, height = 50, gap = 20 },
            children = {
                ui.text { text = label, style = { width = 120, height = 50, foreground = '#ffffff' } },
                ui.stack {
                    style = { width = 300, height = 40, align = 'start', justify = 'start' },
                    children = {
                        ui.path {
                            data = function()
                                local value = motion:get()
                                if reject and value > 40 then return 'invalid path' end
                                return string.format('M0 0H%fV40H0Z', value)
                            end,
                            view_box = { x = 0, y = 0, width = 300, height = 40 },
                            style = { width = 300, height = 40,
                                foreground = if reloaded then '#7997ff' else '#28b3a1' },
                        },
                        ui.row { style = {
                            width = 8, height = 40,
                            translate_x = function() return motion:get() + 8 end,
                            background = function()
                                return if motion:velocity() == 0 then '#47d16c' else '#e84bb5'
                            end,
                        } },
                    },
                },
            },
        }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 260, padding = 20, background = '#0d1117' },
        children = { ui.column {
            style = { width = 600, height = 220, gap = 20 },
            children = {
                sample('transition', { kind = 'transition', duration = 450, easing = 'linear' },
                    if reloaded then 180 else 120),
                sample('retarget', { kind = 'spring', stiffness = 100, damping = 1, epsilon = 0.01 },
                    160, if reloaded then 140 else 80),
                sample('recovery', { kind = 'transition', duration = 300, easing = 'linear' },
                    100, 20, true),
            },
        } },
    }
end
