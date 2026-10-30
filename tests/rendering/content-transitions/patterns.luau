local colors = { alpha = '#e84bb5', beta = '#28b3a1', gamma = '#7997ff' }

local function replacement()
    local shown, requested, phase = ui.state('alpha'), ui.state('alpha'), ui.state('idle')
    local fade = ui.motion(1, { kind = 'transition', duration = 180, easing = 'linear' })
    local pump = ui.timer {
        interval = 20, repeating = true,
        on_timeout = function(timer)
            if phase:get() == 'closing' then
                if not fade:running() and fade:get() == 0 then
                    shown:set(requested:get())
                    phase:set('opening')
                    fade:to(1)
                else
                    fade:to(0)
                end
            elseif phase:get() == 'opening' then
                if not fade:running() and fade:get() == 1 then
                    phase:set('idle')
                    timer:stop()
                else
                    fade:to(1)
                end
            else
                timer:stop()
            end
        end,
    }
    local function select(value)
        requested:set(value)
        local returning = value == shown:get()
        phase:set(if returning then 'opening' else 'closing')
        fade:to(if returning then 1 else 0)
        pump:start()
    end
    local view = ui.stack {
        key = 'replacement', style = { width = 100, height = 60 },
        children = function()
            local value = shown:get()
            return { ui.row { key = value, style = {
                width = 100, height = 60, background = colors[value],
                opacity = function() return math.clamp(fade:get(), 0, 1) end,
            } } }
        end,
    }
    return { select = select, view = view, shown = shown, requested = requested, phase = phase, fade = fade }
end

local function crossfade()
    local selected = ui.state(false)
    local mix = ui.motion(0, { kind = 'transition', duration = 300, easing = 'linear' })
    local function select(value)
        selected:set(value)
        mix:to(if value then 1 else 0)
    end
    local view = ui.stack {
        key = 'crossfade', style = { width = 100, height = 60 },
        children = function()
            local children = {}
            if not selected:get() or mix:running() or mix:get() ~= 1 then
                table.insert(children, ui.row { key = 'alpha', style = {
                    width = 100, height = 60, background = colors.alpha,
                } })
            end
            if selected:get() or mix:running() or mix:get() ~= 0 then
                table.insert(children, ui.row { key = 'beta', style = {
                    width = 100, height = 60, background = colors.beta,
                    opacity = function() return math.clamp(mix:get(), 0, 1) end,
                } })
            end
            return children
        end,
    }
    return { select = select, view = view, mix = mix, selected = selected }
end

return { replacement = replacement, crossfade = crossfade }
