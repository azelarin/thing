return function(reloaded, autoplay)
    local patterns = require('./patterns')
    local sequence, blend = patterns.replacement(), patterns.crossfade()
    local function button(key, callback)
        return ui.button { key = key, on_click = callback, style = { width = 8, height = 8 } }
    end
    if autoplay ~= false then
        ui.timer { interval = 1, autostart = true, on_timeout = function()
            sequence.select('beta'); blend.select(true)
        end }
        ui.timer { interval = 70, autostart = true, on_timeout = function()
            sequence.select('gamma'); blend.select(false)
        end }
        ui.timer { interval = 110, autostart = true, on_timeout = function()
            sequence.select('alpha'); blend.select(true)
        end }
        ui.timer { interval = 160, autostart = true, on_timeout = function()
            sequence.select(if reloaded then 'beta' else 'gamma')
            blend.select(not reloaded)
        end }
    end
    return ui.panel {
        anchors = { top = true, left = true },
        style = { width = 640, height = 240, padding = 20, background = '#0d1117' },
        children = { ui.column {
            style = { width = 600, height = 200, gap = 20, align = 'start' },
            children = {
                ui.row { style = { width = 600, height = 60, gap = 20 }, children = {
                    sequence.view, blend.view,
                    ui.stack { style = { width = 100, height = 60 }, children = {
                        ui.row { style = { width = 100, height = 60, background = '#e84bb5' } },
                        ui.row { style = { width = 100, height = 60, background = '#28b3a1', opacity = 0.5 } },
                    } },
                } },
                ui.row { style = { width = 600, height = 20, gap = 8 }, children = {
                    button('select-alpha', function() sequence.select('alpha') end),
                    button('select-beta', function() sequence.select('beta') end),
                    button('select-gamma', function() sequence.select('gamma') end),
                    button('blend-alpha', function() blend.select(false) end),
                    button('blend-beta', function() blend.select(true) end),
                    button('fail', function() sequence.select('beta'); blend.select(true); error('rejected selection') end),
                } },
                ui.row { style = { width = 600, height = 20, gap = 8 }, children = {
                    ui.text { key = 'shown', text = function() return sequence.shown:get() end },
                    ui.text { key = 'requested', text = function() return sequence.requested:get() end },
                    ui.text { key = 'phase', text = function() return sequence.phase:get() end },
                    ui.text { key = 'fade', text = function() return tostring(sequence.fade:get()) end },
                    ui.text { key = 'mix', text = function() return tostring(blend.mix:get()) end },
                } },
            },
        } },
    }
end
