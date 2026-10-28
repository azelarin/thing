--!strict

local properties: ThingSecretInputProperties = {}
properties.value = "secret"
properties.on_change = function(_value) end
properties.on_submit = function(_value) end

return ui.lock { children = { ui.secret_input(properties) } }
