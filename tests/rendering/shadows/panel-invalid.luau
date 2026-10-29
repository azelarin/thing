-- valid luau whose shadow fails runtime validation, so the host must keep the
-- previous surfaces.
return require('./gallery')({ shadow = { color = '#000000', offset_y = -20, blur = 40 } })
