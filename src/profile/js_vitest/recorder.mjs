// The host supplies a fresh report file and reads it separately from Vitest's logs.
import fs from 'node:fs'
import path from 'node:path'
import { createRequire } from 'node:module'
import { pathToFileURL } from 'node:url'

const [file, pattern, explicitConfig] = process.argv.slice(1)
const reportPath = process.env.LYRA_COMMAND_REPORT
const cacheDir = process.env.LYRA_VITEST_CACHE
const mode = process.env.LYRA_VITEST_MODE || 'regression'
delete process.env.LYRA_COMMAND_REPORT
delete process.env.LYRA_VITEST_MODE

const realpath = fs.realpathSync
const writeFile = fs.writeFileSync
const root = realpath(process.cwd())
const relative = (base, p) => path.relative(base, p).split(path.sep).join('/')
const contains = (base, p) => {
  const rel = relative(base, p)
  return rel !== '..' && !rel.startsWith('../') && !path.isAbsolute(rel)
}
const errors = task => (task.result?.errors || []).slice(0, 10).map(error => ({
  name: String(error?.name || ''),
  message: String(error?.message || error).slice(0, 2000),
}))

function resolvedConfig() {
  if (explicitConfig) return checkoutConfig(path.resolve(root, explicitConfig))
  for (const stem of ['vitest.config', 'vite.config']) {
    for (const extension of ['ts', 'mts', 'cts', 'js', 'mjs', 'cjs']) {
      const config = path.join(root, stem + '.' + extension)
      if (fs.existsSync(config)) return checkoutConfig(config)
    }
  }
  // createVitest otherwise searches ancestors and can execute an unrelated config.
  return false
}

function checkoutConfig(config) {
  if (!fs.existsSync(config)) throw new Error('Vitest config is missing from the checkout: ' + config)
  const real = realpath(config)
  if (!contains(root, real) || !fs.statSync(real).isFile()) {
    throw new Error('Vitest config must be a file inside the checkout: ' + config)
  }
  return config
}

function summarize(files) {
  const matcher = mode === 'regression' ? new RegExp(pattern) : null
  const tests = []
  const suiteErrors = []
  let passed = 0
  let failed = 0
  let firstFailure
  function walk(task, file, parents) {
    if (task.type === 'test') {
      const state = task.result?.state || null
      if (state === 'pass') passed++
      if (state === 'fail') {
        failed++
        firstFailure ||= [file, [...parents, task.name].join(' > '), errors(task)[0]?.message]
      }
      if (mode === 'discover' || (matcher && (state === 'pass' || state === 'fail' || matcher.test([...parents, task.name].join(' '))))) {
        tests.push({ file, name: [...parents, task.name], state, errors: errors(task), inverted: !!task.fails })
      }
      return
    }
    const taskErrors = errors(task)
    if (taskErrors.length && mode !== 'suite') suiteErrors.push({ file, errors: taskErrors })
    if (taskErrors.length) {
      failed++
      firstFailure ||= [file, [...parents, task.name].join(' > '), taskErrors[0].message]
    }
    const names = task === task.file ? parents : [...parents, task.name]
    for (const child of task.tasks || []) walk(child, file, names)
  }
  for (const file of files) walk(file, relative(root, file.filepath), [])
  return { tests, suiteErrors, passed, failed, firstFailure }
}

function collectPaths(project, dependencies, config) {
  const loaded = new Set()
  const setup = new Set()
  const foreign = new Set()
  function record(id, target) {
    if (typeof id !== 'string' || id.startsWith('\0')) return
    if (!path.isAbsolute(id)) return
    const filename = fs.existsSync(id) ? id : id.split('?')[0]
    let real
    try { real = realpath(filename) } catch { return }
    if (contains(root, real)) {
      target.add(relative(root, real))
      if (target === setup && contains(root, filename)) target.add(relative(root, filename))
    } else if (!contains(dependencies, real)) {
      foreign.add(real.split(path.sep).join('/'))
    }
  }
  for (const entry of [
    ...(project.config.setupFiles || []),
    ...(project.config.globalSetup || []),
    ...(project.vite.config.configFileDependencies || []),
    project.vite.config.configFile,
    config,
  ]) record(entry, setup)
  const environments = project.vite.environments
  const graphs = [environments?.ssr?.moduleGraph, environments?.__vitest__?.moduleGraph].filter(Boolean)
  if (!graphs.length && project.vite.moduleGraph) graphs.push(project.vite.moduleGraph)
  if (!graphs.length || graphs.some(graph => !graph.idToModuleMap?.keys)) {
    throw new Error('Vitest did not expose its module graph')
  }
  for (const graph of graphs) {
    for (const [id, module] of graph.idToModuleMap) record(module.file || id, loaded)
  }
  return { loaded: [...loaded].sort(), setup: [...setup].sort(), foreign: [...foreign].sort() }
}

let vitest
let exitCode = 2
try {
  if (!reportPath) throw new Error('LYRA did not configure Vitest evidence')
  const dependencies = realpath(path.join(root, '..', 'node_modules'))
  const require = createRequire(path.join(dependencies, '.lyra-vitest.cjs'))
  const { version } = require('vitest/package.json')
  if (!/^3\./.test(version)) throw new Error('LYRA supports Vitest 3.x; found ' + version)
  const vitestNode = realpath(require.resolve('vitest/node'))
  if (!contains(dependencies, vitestNode)) throw new Error('Vitest must resolve from the isolated dependencies')
  const { createVitest } = await import(pathToFileURL(vitestNode).href)
  const config = resolvedConfig()
  process.env.TEST = 'true'
  process.env.VITEST = 'true'
  process.env.NODE_ENV ??= 'test'
  vitest = await createVitest('test', {
    config,
    watch: false,
    color: false,
    passWithNoTests: mode === 'discover',
    testNamePattern: mode === 'regression' ? pattern : undefined,
    reporters: mode === 'suite' ? [{}] : ['default'],
    allowOnly: false,
    update: false,
  }, { cacheDir, logLevel: 'error' })

  if (vitest.projects.length !== 1 || vitest.config.projects?.length || vitest.config.workspace) {
    throw new Error('Vitest projects and workspaces are not supported yet (Phase 3b)')
  }
  const [project] = vitest.projects
  if (!contains(root, realpath(project.config.root))) {
    throw new Error('Vitest project root must stay inside the checkout')
  }
  if (project.config.browser?.enabled) throw new Error('LYRA requires a Vitest Node project')
  if (mode === 'suite') {
    await vitest.start()
  } else {
    const selected = realpath(path.resolve(root, file))
    const specs = (await vitest.globTestSpecifications([selected])).filter(spec => {
      return path.relative(selected, realpath(spec.moduleId)) === ''
    })
    if (specs.length !== 1) throw new Error('Vitest must discover exactly one specification for ' + file)
    if (mode === 'discover') await vitest.collectTests(specs)
    else {
      await vitest.report('onInit', vitest)
      await vitest.runTestSpecifications(specs)
    }
  }
  const files = vitest.state.getFiles()
  const summary = summarize(files)
  const unhandled = vitest.state.getUnhandledErrors().length
  const paths = collectPaths(project, dependencies, config)
  if (mode !== 'suite' && !summary.failed && !unhandled
      && !paths.loaded.includes(relative(root, realpath(path.resolve(root, file))))) {
    throw new Error('Vitest did not record the selected test file in its module graph: ' + file)
  }
  if (mode === 'suite') {
    console.log('Vitest suite: ' + summary.passed + ' passed, ' + summary.failed + ' failed, ' + unhandled + ' unhandled errors')
    if (summary.firstFailure) console.error('FAIL ' + summary.firstFailure.filter(Boolean).join(' > ').slice(0, 1000))
    if (paths.foreign.length) console.error('FAIL Vitest resolved a module from the operator checkout: ' + paths.foreign[0])
  }
  await vitest.close()
  vitest = null
  exitCode = process.exitCode ?? 0
  if (summary.failed || unhandled || paths.foreign.length || (mode === 'suite' && !summary.passed)) exitCode = 1
  const report = mode === 'suite'
    ? { version: 1, mode }
    : { version: 1, mode, collected: files.map(file => relative(root, file.filepath)), tests: summary.tests, suiteErrors: summary.suiteErrors, unhandled, ...paths }
  writeFile(reportPath, JSON.stringify(report), { encoding: 'utf8', flag: 'r+' })
} catch (error) {
  console.error('LYRA-STARTUP-ERROR ' + String(error?.stack || error))
  if (vitest) {
    try { await vitest.close() } catch (error) { console.error(error) }
  }
  exitCode = 2
}
// Flush both captured streams before exiting even if a test left a timer running.
await Promise.all([process.stdout, process.stderr].map(stream => new Promise(resolve => stream.write('', resolve))))
process.exit(exitCode)
